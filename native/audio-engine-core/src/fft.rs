/// FFT 频谱分析器
///
/// 当 `fft` feature 开启时（Electron / 桌面端），使用 rustfft 进行真实频谱分析。
/// 当 `fft` feature 关闭时（headless 服务端），提供零成本 stub，所有调用均为空操作，
/// 不链接 rustfft，避免额外 4–6 MB 代码段。

#[cfg(feature = "fft")]
mod real {
    use parking_lot::Mutex;
    use rustfft::{num_complex::Complex, Fft, FftPlanner};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const FFT_SIZE: usize = 2048;
    const FFT_SAMPLE_RATE: u32 = 48_000;
    const OUTPUT_BINS: usize = 128;
    const MIN_FREQ: f32 = 80.0;
    const MAX_FREQ: f32 = 2000.0;
    const MAX_BUFFER_SIZE: usize = 8192;

    pub struct FftAnalyzer {
        sample_buffer: Mutex<StereoSampleBuffer>,
        enabled: AtomicBool,
        fft_plan: Arc<dyn Fft<f32>>,
        window: Vec<f32>,
        work: Mutex<FftWorkBuffers>,
    }

    struct StereoSampleBuffer {
        left: Vec<f32>,
        right: Vec<f32>,
        write_pos: usize,
        len: usize,
    }

    struct FftWorkBuffers {
        windowed_l: Vec<Complex<f32>>,
        windowed_r: Vec<Complex<f32>>,
        output_l: Vec<f32>,
        output_r: Vec<f32>,
    }

    impl FftAnalyzer {
        pub fn new() -> Self {
            let mut planner = FftPlanner::<f32>::new();
            let fft_plan = planner.plan_fft_forward(FFT_SIZE);
            let window = (0..FFT_SIZE)
                .map(|i| {
                    0.54 - 0.46
                        * (2.0 * std::f32::consts::PI * i as f32 / (FFT_SIZE as f32 - 1.0)).cos()
                })
                .collect();

            Self {
                sample_buffer: Mutex::new(StereoSampleBuffer {
                    left: vec![0.0; MAX_BUFFER_SIZE],
                    right: vec![0.0; MAX_BUFFER_SIZE],
                    write_pos: 0,
                    len: 0,
                }),
                enabled: AtomicBool::new(false),
                fft_plan,
                window,
                work: Mutex::new(FftWorkBuffers {
                    windowed_l: vec![Complex::new(0.0, 0.0); FFT_SIZE],
                    windowed_r: vec![Complex::new(0.0, 0.0); FFT_SIZE],
                    output_l: vec![0.0; OUTPUT_BINS],
                    output_r: vec![0.0; OUTPUT_BINS],
                }),
            }
        }

        pub fn push_interleaved_samples(&self, interleaved: &[f32]) {
            let mut buffer = self.sample_buffer.lock();
            for pair in interleaved.chunks_exact(2) {
                let write_pos = buffer.write_pos;
                buffer.left[write_pos] = pair[0];
                buffer.right[write_pos] = pair[1];
                buffer.write_pos = (write_pos + 1) % MAX_BUFFER_SIZE;
                buffer.len = (buffer.len + 1).min(MAX_BUFFER_SIZE);
            }
        }

        pub fn set_enabled(&self, enabled: bool) {
            let was_enabled = self.enabled.swap(enabled, Ordering::Relaxed);
            if enabled && !was_enabled {
                self.reset();
            }
        }

        pub fn is_enabled(&self) -> bool {
            self.enabled.load(Ordering::Relaxed)
        }

        fn apply_window(&self, samples: &[f32], start: usize, windowed: &mut [Complex<f32>]) {
            for (i, output) in windowed.iter_mut().enumerate() {
                let sample = samples[(start + i) % MAX_BUFFER_SIZE];
                *output = Complex::new(sample * self.window[i], 0.0);
            }
        }

        fn to_normalized_db(&self, avg: f32) -> f32 {
            let db = 20.0 * (avg + 1e-10).log10();
            ((db + 60.0) / 60.0).clamp(0.0, 1.0)
        }

        pub fn analyze(&self) -> (Vec<f32>, Vec<f32>) {
            let buffer = self.sample_buffer.lock();
            if buffer.len < FFT_SIZE {
                return (vec![0.0; OUTPUT_BINS], vec![0.0; OUTPUT_BINS]);
            }
            let start = (buffer.write_pos + MAX_BUFFER_SIZE - FFT_SIZE) % MAX_BUFFER_SIZE;
            let mut work = self.work.lock();
            self.apply_window(&buffer.left, start, &mut work.windowed_l);
            self.apply_window(&buffer.right, start, &mut work.windowed_r);
            drop(buffer);
            self.fft_plan.process(&mut work.windowed_l);
            self.fft_plan.process(&mut work.windowed_r);
            let freq_per_bin = FFT_SAMPLE_RATE as f32 / FFT_SIZE as f32;
            let min_bin = (MIN_FREQ / freq_per_bin).floor() as usize;
            let max_bin = ((MAX_FREQ / freq_per_bin).ceil() as usize).min(FFT_SIZE / 2);
            if min_bin >= max_bin {
                work.output_l.iter_mut().for_each(|v| *v = 0.0);
                work.output_r.iter_mut().for_each(|v| *v = 0.0);
                return (work.output_l.clone(), work.output_r.clone());
            }
            let log_min = MIN_FREQ.ln();
            let log_max = MAX_FREQ.ln();
            for i in 0..OUTPUT_BINS {
                let freq_lo =
                    (log_min + (log_max - log_min) * i as f32 / OUTPUT_BINS as f32).exp();
                let freq_hi =
                    (log_min + (log_max - log_min) * (i + 1) as f32 / OUTPUT_BINS as f32).exp();
                let bin_lo = ((freq_lo / freq_per_bin).floor() as usize).max(min_bin);
                let bin_hi = ((freq_hi / freq_per_bin).ceil() as usize).min(max_bin);
                if bin_lo >= bin_hi {
                    work.output_l[i] = 0.0;
                    work.output_r[i] = 0.0;
                    continue;
                }
                let mut sums: (f32, f32) = (0.0, 0.0);
                for j in bin_lo..bin_hi {
                    sums.0 += work.windowed_l[j].norm() / FFT_SIZE as f32;
                    sums.1 += work.windowed_r[j].norm() / FFT_SIZE as f32;
                }
                let avgs = (
                    sums.0 / (bin_hi - bin_lo) as f32,
                    sums.1 / (bin_hi - bin_lo) as f32,
                );
                work.output_l[i] = self.to_normalized_db(avgs.0);
                work.output_r[i] = self.to_normalized_db(avgs.1);
            }
            (work.output_l.clone(), work.output_r.clone())
        }

        pub fn reset(&self) {
            let mut buffer = self.sample_buffer.lock();
            buffer.write_pos = 0;
            buffer.len = 0;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn interleaved_samples_wrap_without_mixing_channels() {
            let analyzer = FftAnalyzer::new();
            let samples: Vec<f32> = (0..MAX_BUFFER_SIZE + 16)
                .flat_map(|i| [i as f32, -(i as f32)])
                .collect();
            analyzer.push_interleaved_samples(&samples);
            let buffer = analyzer.sample_buffer.lock();
            assert_eq!(buffer.len, MAX_BUFFER_SIZE);
            assert_eq!(buffer.write_pos, 16);
            let latest = (buffer.write_pos + MAX_BUFFER_SIZE - 1) % MAX_BUFFER_SIZE;
            assert_eq!(buffer.left[latest], (MAX_BUFFER_SIZE + 15) as f32);
            assert_eq!(buffer.right[latest], -((MAX_BUFFER_SIZE + 15) as f32));
        }

        #[test]
        fn reset_discards_buffered_samples() {
            let analyzer = FftAnalyzer::new();
            analyzer.push_interleaved_samples(&[0.5, -0.5, 0.25, -0.25]);
            analyzer.reset();
            let buffer = analyzer.sample_buffer.lock();
            assert_eq!(buffer.len, 0);
            assert_eq!(buffer.write_pos, 0);
        }

        #[test]
        fn fixed_sample_rate_maps_tone_to_expected_band() {
            let analyzer = FftAnalyzer::new();
            let frequency = 1_000.0;
            let samples: Vec<f32> = (0..FFT_SIZE)
                .flat_map(|i| {
                    let phase = 2.0
                        * std::f32::consts::PI
                        * frequency
                        * i as f32
                        / FFT_SAMPLE_RATE as f32;
                    let sample = phase.sin();
                    [sample, sample]
                })
                .collect();
            analyzer.push_interleaved_samples(&samples);
            let (left, right) = analyzer.analyze();
            let peak = left
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(index, _)| index)
                .unwrap();
            let expected = ((frequency.ln() - MIN_FREQ.ln())
                / (MAX_FREQ.ln() - MIN_FREQ.ln())
                * OUTPUT_BINS as f32) as usize;
            assert!(
                peak.abs_diff(expected) <= 1,
                "peak={peak}, expected={expected}"
            );
            assert_eq!(left, right);
        }
    }
}

#[cfg(feature = "fft")]
pub use real::FftAnalyzer;

// ---------------------------------------------------------------------------
// Stub（headless 模式，fft feature 关闭时使用）
// 所有方法均为空操作，编译器会完全内联并消除，无运行期开销，不链接 rustfft。
// ---------------------------------------------------------------------------
#[cfg(not(feature = "fft"))]
pub struct FftAnalyzer;

#[cfg(not(feature = "fft"))]
impl FftAnalyzer {
    #[inline(always)]
    pub fn new() -> Self {
        Self
    }

    #[inline(always)]
    pub fn push_interleaved_samples(&self, _interleaved: &[f32]) {}

    #[inline(always)]
    pub fn set_enabled(&self, _enabled: bool) {}

    #[inline(always)]
    pub fn is_enabled(&self) -> bool {
        false
    }

    #[inline(always)]
    pub fn analyze(&self) -> (Vec<f32>, Vec<f32>) {
        (Vec::new(), Vec::new())
    }

    #[inline(always)]
    pub fn reset(&self) {}
}
