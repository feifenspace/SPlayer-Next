use std::cell::UnsafeCell;
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
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

    // 通用多声道下混
    for i in 0..samples {
        let idx = start_sample + i;
        let mut l = *planes[0].add(idx) as f32;
        let mut r = *planes[1].add(idx) as f32;
        if channels > 2 && !planes[2].is_null() {
            let c = *planes[2].add(idx) as f32;
            l += INV_SQRT2_F32 * c;
            r += INV_SQRT2_F32 * c;
        }
        for ch in 3..channels.min(8) {
            if !planes[ch].is_null() {
                let s = *planes[ch].add(idx) as f32;
                if ch % 2 == 1 { l += 0.5 * s; } else { r += 0.5 * s; }
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

    // 通用多声道下混
    for i in 0..samples {
        let idx = start_sample + i;
        let mut l = *planes[0].add(idx) as f64;
        let mut r = *planes[1].add(idx) as f64;
        if channels > 2 && !planes[2].is_null() {
            let c = *planes[2].add(idx) as f64;
            l += INV_SQRT2_F64 * c;
            r += INV_SQRT2_F64 * c;
        }
        for ch in 3..channels.min(8) {
            if !planes[ch].is_null() {
                let s = *planes[ch].add(idx) as f64;
                if ch % 2 == 1 { l += 0.5 * s; } else { r += 0.5 * s; }
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

    for i in 0..samples {
        let idx = start_sample + i;
        let mut l = *planes[0].add(idx);
        let mut r = *planes[1].add(idx);
        if channels > 2 && !planes[2].is_null() {
            let c = *planes[2].add(idx);
            l += INV_SQRT2_F32 * c;
            r += INV_SQRT2_F32 * c;
        }
        for ch in 3..channels.min(8) {
            if !planes[ch].is_null() {
                let s = *planes[ch].add(idx);
                if ch % 2 == 1 { l += 0.5 * s; } else { r += 0.5 * s; }
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
    for i in 0..samples {
        let base = (start_sample + i) * channels;
        let fl = *ptr.add(base) as f32;
        let fr = *ptr.add(base + 1) as f32;
        let fc = if channels > 2 { *ptr.add(base + 2) as f32 } else { 0.0 };
        let mut l = fl + INV_SQRT2_F32 * fc;
        let mut r = fr + INV_SQRT2_F32 * fc;
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
                if ch % 2 == 1 { l += 0.5 * s; } else { r += 0.5 * s; }
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
    for i in 0..samples {
        let base = (start_sample + i) * channels;
        let fl = *ptr.add(base) as f64;
        let fr = *ptr.add(base + 1) as f64;
        let fc = if channels > 2 { *ptr.add(base + 2) as f64 } else { 0.0 };
        let mut l = fl + INV_SQRT2_F64 * fc;
        let mut r = fr + INV_SQRT2_F64 * fc;
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
                if ch % 2 == 1 { l += 0.5 * s; } else { r += 0.5 * s; }
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
    for i in 0..samples {
        let base = (start_sample + i) * channels;
        let fl = *ptr.add(base);
        let fr = *ptr.add(base + 1);
        let fc = if channels > 2 { *ptr.add(base + 2) } else { 0.0 };
        let mut l = fl + INV_SQRT2_F32 * fc;
        let mut r = fr + INV_SQRT2_F32 * fc;
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
                if ch % 2 == 1 { l += 0.5 * s; } else { r += 0.5 * s; }
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
        let path = path
            .to_str()
            .context("Source Direct 当前只接受 UTF-8 本地路径")?;
        let path = CString::new(path).context("Source Direct 本地路径包含 NUL")?;

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
const SLOT_FREE: u8 = 0;
const SLOT_FILLING: u8 = 1;
const SLOT_READY: u8 = 2;
const SLOT_IN_FLIGHT: u8 = 3;
const NO_SLOT: usize = usize::MAX;

struct DirectPcmSlot {
    state: AtomicU8,
    payload_ptr: AtomicPtr<u8>,
    payload_len: AtomicUsize,
    sample_frames: AtomicUsize,
    boundary: AtomicBool,
    boundary_duration_micros: AtomicU64,
    boundary_generation: AtomicU64,
    frame: UnsafeCell<DirectPcmFrame>,
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
        })
    }
}

// slot 的 frame 只在 FILLING 时由 producer 修改，在 READY/IN_FLIGHT 时只读。
unsafe impl Sync for DirectPcmSlot {}

enum DirectPcmCommand {
    Seek {
        position_secs: f64,
        response: mpsc::SyncSender<Result<f64>>,
    },
    ReplaceLocal {
        path: PathBuf,
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
        })
    }

    fn next_block(&self) -> Option<DirectPcmBlock> {
        self.release_in_flight();
        if self.failed.load(Ordering::Acquire) {
            return None;
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
    }

    fn reset_for_transition(&self) {
        self.release_in_flight();
        self.consumer_index.store(0, Ordering::Relaxed);
        self.in_flight.store(NO_SLOT, Ordering::Relaxed);
        self.consumed_frames.store(0, Ordering::Relaxed);
        self.finished.store(false, Ordering::Relaxed);
        self.failed.store(false, Ordering::Relaxed);
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
    }
}

fn seek_pcm_ring(
    decoder: &mut DirectPcmDecoder,
    ring: &DirectPcmRing,
    expected_format: DirectPcmFormat,
    position_secs: f64,
) -> Result<f64> {
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
    path: &Path,
    ring: &DirectPcmRing,
    current_format: DirectPcmFormat,
) -> Result<(DirectPcmDecoder, DirectPcmFormat)> {
    let mut decoder = DirectPcmDecoder::open_local(path)?;
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
    first_slot.state.store(SLOT_READY, Ordering::Release);
    Ok((decoder, new_format))
}

#[derive(Clone)]
pub struct DirectPcmMonitor {
    ring: Arc<DirectPcmRing>,
    sample_rate: u32,
}

impl DirectPcmMonitor {
    pub fn consumed_position(&self) -> f64 {
        self.ring.consumed_frames.load(Ordering::Acquire) as f64 / f64::from(self.sample_rate)
    }

    pub fn failed(&self) -> bool {
        self.ring.failed.load(Ordering::Acquire)
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
pub struct DirectPcmStageHandle {
    control_tx: mpsc::Sender<DirectPcmCommand>,
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
        response_rx
            .recv()
            .context("等待 Source Direct PCM staged source 结果失败")?
    }

    pub fn cancel(&self) {
        let _ = self.control_tx.send(DirectPcmCommand::CancelStaged);
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
                // 防止 PCM 解码推流线程被调度到效率核或超线程虚拟核造成推流抗跟　2
                bind_current_thread_to_performance_cores("diretta-direct-decode");
                boost_current_audio_thread("diretta-direct-decode");
                let mut active_format = format;
                let mut staged: Option<StagedPcmSource> = None;
                let mut next_slot = 1 % producer_ring.slots.len();
                while !producer_ring.stopped.load(Ordering::Acquire) {
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
                            next_slot = 1 % producer_ring.slots.len();
                            continue;
                        }
                        Ok(DirectPcmCommand::ReplaceLocal { path, response }) => {
                            let result = replace_pcm_ring(&path, &producer_ring, active_format);
                            match result {
                                Ok((new_decoder, new_format)) => {
                                    decoder = new_decoder;
                                    active_format = new_format;
                                    staged = None;
                                    let _ = response.send(Ok(new_format));
                                    next_slot = 1 % producer_ring.slots.len();
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
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    if producer_ring.finished.load(Ordering::Acquire) {
                        let Some(candidate) = staged.take() else {
                            thread::sleep(Duration::from_millis(1));
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
                            thread::sleep(Duration::from_millis(1));
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
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }

                    let frame = unsafe { &mut *slot.frame.get() };
                    match decoder.read_frame(frame) {
                        Ok(true) => {
                            let frame_format = match frame.format() {
                                Ok(value) => value,
                                Err(_) => {
                                    slot.state.store(SLOT_FREE, Ordering::Release);
                                    producer_ring.failed.store(true, Ordering::Release);
                                    return;
                                }
                            };
                            if frame_format != active_format {
                                slot.state.store(SLOT_FREE, Ordering::Release);
                                producer_ring.failed.store(true, Ordering::Release);
                                return;
                            }
                            let data = match frame.payload_ptr() {
                                Ok(value) => value,
                                Err(_) => {
                                    slot.state.store(SLOT_FREE, Ordering::Release);
                                    producer_ring.failed.store(true, Ordering::Release);
                                    return;
                                }
                            };
                            slot.payload_ptr.store(data.cast_mut(), Ordering::Relaxed);
                            slot.payload_len.store(frame.payload_len, Ordering::Relaxed);
                            slot.sample_frames
                                .store(frame.samples_per_channel(), Ordering::Relaxed);
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
                        Err(_) => {
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
        response_rx
            .recv()
            .context("等待 Source Direct PCM seek 结果失败")?
    }

    pub fn replace_local_while_paused(&mut self, path: &Path) -> Result<DirectPcmFormat> {
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectPcmCommand::ReplaceLocal {
                path: path.to_owned(),
                response: response_tx,
            })
            .context("提交 Source Direct PCM handoff 失败")?;
        let format = response_rx
            .recv()
            .context("等待 Source Direct PCM handoff 结果失败")??;
        self.format = format;
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

    use base64::Engine;

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

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
            .replace_local_while_paused(&replacement.path)
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
            .replace_local_while_paused(&incompatible.path)
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
}

