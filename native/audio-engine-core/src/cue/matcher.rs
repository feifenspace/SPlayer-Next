//! CUE 音频目标文件智能匹配器
//!
//! 支持以下解析与匹配策略（移植并增强自 tinyLMS-old）：
//! 1. 路径分隔符标准化（Windows `\` 转换为 Unix `/`）；
//! 2. 精确路径存在性检查；
//! 3. 大小写不敏感文件名匹配；
//! 4. 忽略标点符号与空格的模糊归一化匹配；
//! 5. 目录内单音频文件智能兜底。

use std::fs;
use std::path::{Path, PathBuf};

/// 常见的音频扩展名列表
const KNOWN_AUDIO_EXTENSIONS: &[&str] = &[
    "ape", "flac", "wav", "mp3", "m4a", "ogg", "wma", "opus", "aac", "dsf", "dff", "iso", "cue",
];

/// 对字符串进行模糊归一化处理（仅保留非 ASCII 字符与 ASCII 字母数字，转为小写）
pub fn normalize_for_fuzzy_match(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        if !c.is_ascii() || c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                result.push(lc);
            }
        }
    }
    result
}

/// 解析并定位 CUE 中 FILE 指令所引用的真实音频文件路径
pub fn resolve_audio_path<P: AsRef<Path>>(cue_dir: P, audio_filename: &str) -> PathBuf {
    let cue_dir = cue_dir.as_ref();

    // 1. 标准化路径分隔符
    let rel_path_str = audio_filename.replace('\\', "/");
    let target = cue_dir.join(&rel_path_str);

    // 2. 精确匹配
    if target.exists() {
        return target;
    }

    // 提取纯文件名
    let file_name = match Path::new(&rel_path_str).file_name() {
        Some(name) => name.to_string_lossy(),
        None => return target,
    };
    let file_name_lower = file_name.to_lowercase();

    let read_dir = match fs::read_dir(cue_dir) {
        Ok(entries) => entries,
        Err(_) => return target,
    };

    let mut dir_files: Vec<PathBuf> = Vec::new();
    for entry in read_dir.flatten() {
        if let Ok(file_type) = entry.file_type() {
            if file_type.is_file() {
                dir_files.push(entry.path());
            }
        }
    }

    // 3. 大小写不敏感匹配
    for path in &dir_files {
        if let Some(entry_name) = path.file_name() {
            if entry_name.to_string_lossy().to_lowercase() == file_name_lower {
                return path.clone();
            }
        }
    }

    // 4. 忽略标点和空格的模糊匹配
    let fuzzy_target = normalize_for_fuzzy_match(&file_name);
    if !fuzzy_target.is_empty() {
        for path in &dir_files {
            if let Some(entry_name) = path.file_name() {
                let entry_str = entry_name.to_string_lossy();
                if normalize_for_fuzzy_match(&entry_str) == fuzzy_target {
                    return path.clone();
                }
            }
        }
    }

    // 5. 兜底：如果目录下仅存在一个支持的音频母版文件，则直接选用该文件
    let candidate_audio_files: Vec<&PathBuf> = dir_files
        .iter()
        .filter(|p| {
            p.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| {
                    !ext.eq_ignore_ascii_case("cue")
                        && KNOWN_AUDIO_EXTENSIONS
                            .iter()
                            .any(|&e| e.eq_ignore_ascii_case(ext))
                })
                .unwrap_or(false)
        })
        .collect();

    if candidate_audio_files.len() == 1 {
        return candidate_audio_files[0].clone();
    }

    // 未找到匹配，返回基于 cue_dir 的原始构造路径
    target
}
