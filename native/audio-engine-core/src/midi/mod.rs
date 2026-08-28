//! MIDI (.mid) 纯 Rust 软波表实时合成与渲染器
//!
//! 基于 `rustysynth`（100% Safe Rust SoundFont 2 引擎）。
//! 具备以下能力：
//! 1. 加载并解析标准 MIDI 文件 (.mid, .midi)；
//! 2. 加载 SoundFont 2 音色库 (.sf2)；
//! 3. 实时多复音高质量立体声渲染输出。

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};

/// 检查路径是否为 MIDI 文件
pub fn is_midi_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".mid") || lower.ends_with(".midi")
}

/// 纯 Rust MIDI 实时音频渲染器
pub struct MidiRenderer {
    pub sample_rate: u32,
    pub duration_seconds: f64,
    sequencer: MidiFileSequencer,
    synthesizer: Synthesizer,
    left_buf: Vec<f32>,
    right_buf: Vec<f32>,
}

impl MidiRenderer {
    /// 打开 MIDI 文件并装载 SoundFont 进行初始化
    pub fn open<P: AsRef<Path>>(
        midi_path: P,
        soundfont_path: Option<&str>,
        sample_rate: u32,
    ) -> Result<Self> {
        let midi_path = midi_path.as_ref();
        let mut midi_file = File::open(midi_path)
            .map_err(|e| anyhow!("Failed to open MIDI file {}: {}", midi_path.display(), e))?;

        let midi = Arc::new(
            MidiFile::new(&mut midi_file)
                .map_err(|e| anyhow!("Failed to parse MIDI file: {}", e))?,
        );

        let soundfont = if let Some(sf_path) = soundfont_path {
            let mut sf_file = File::open(sf_path)
                .map_err(|e| anyhow!("Failed to open SoundFont {}: {}", sf_path, e))?;
            Arc::new(
                SoundFont::new(&mut sf_file)
                    .map_err(|e| anyhow!("Failed to load SoundFont: {}", e))?,
            )
        } else {
            // 如果未指定外部 SoundFont，可使用空的默认合成器或等待用户指定
            return Err(anyhow!("No SoundFont (.sf2) specified for MIDI synthesis"));
        };

        let settings = SynthesizerSettings::new(sample_rate as i32);
        let synthesizer = Synthesizer::new(&soundfont, &settings)
            .map_err(|e| anyhow!("Failed to initialize MIDI synthesizer: {}", e))?;

        let mut sequencer = MidiFileSequencer::new(synthesizer);
        sequencer.play(&midi, false);

        // rustysynth 中 MidiFileSequencer 持有 synthesizer
        // 估算 MIDI 时长（秒）
        let duration_seconds = 180.0; // 默认预估值

        let block_size = 512;
        Ok(Self {
            sample_rate,
            duration_seconds,
            sequencer,
            synthesizer: Synthesizer::new(&soundfont, &settings)
                .map_err(|e| anyhow!("Synthesizer clone failed: {}", e))?,
            left_buf: vec![0.0; block_size],
            right_buf: vec![0.0; block_size],
        })
    }

    /// 渲染交错立体声样本到目标缓冲
    pub fn render_interleaved_stereo(&mut self, out: &mut [f32]) -> usize {
        let pairs = out.len() / 2;
        if pairs == 0 {
            return 0;
        }

        if self.left_buf.len() < pairs {
            self.left_buf.resize(pairs, 0.0);
            self.right_buf.resize(pairs, 0.0);
        }

        self.sequencer
            .render(&mut self.left_buf[..pairs], &mut self.right_buf[..pairs]);

        for i in 0..pairs {
            out[i * 2] = self.left_buf[i];
            out[i * 2 + 1] = self.right_buf[i];
        }

        pairs * 2
    }
}
