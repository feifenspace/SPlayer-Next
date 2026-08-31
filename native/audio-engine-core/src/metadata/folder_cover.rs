//! 从音频文件所在目录查找外部封面图片。
//!
//! 当音频文件没有内嵌封面时，回退到同目录下的常见封面文件。
//! 支持的文件名（不区分大小写）按优先级排列：
//! cover → folder → album → front → <同名> → 目录下唯一图片

use std::path::{Path, PathBuf};

/// 按优先级排列的封面文件名关键词列表（不含扩展名，大小写不敏感）
const COVER_NAMES: &[&str] = &[
    "cover", "folder", "front", "album", "cd", "cd1", "cd2", "cdimage",
    "back", "disc", "封面", "封套", "试音极品", "试音",
];

/// 支持的图片扩展名
const COVER_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp"];

/// 常见的封面子目录名称
const COVER_SUB_DIRS: &[&str] = &[
    "Artwork", "Scans", "Covers", "Pictures", "封面", "artwork", "scans", "covers", "pictures", "scan", "pic",
];

/// 在音频/CUE文件所在目录及子目录、上级目录查找封面图片。
///
/// 查找顺序：
/// 1. 当前目录下固定优先名：cover / folder / front / album / 封面 / 封套 等
/// 2. 子目录（Artwork / Scans / Covers / Pictures / 封面）下的图片文件
/// 3. 与音频文件同名的图片文件
/// 4. 当前目录下任意图片文件（若只有一张或按优先级挑选最佳匹配）
/// 5. 上级目录下的封面图片（排除 root/music 等根目录）
///
/// @param audio_path - 音频或 CUE 文件路径
/// @returns 找到的封面图片路径，未找到返回 None
pub fn find_folder_cover(audio_path: &str) -> Option<PathBuf> {
    let audio = Path::new(audio_path);
    let dir = if audio.is_dir() { audio } else { audio.parent()? };

    if !dir.is_dir() {
        return None;
    }

    // 1. 检查当前目录
    if let Some(path) = search_dir_for_cover(dir, false) {
        return Some(path);
    }

    // 2. 检查子目录 (Artwork, Scans, Covers 等)
    for sub in COVER_SUB_DIRS {
        let sub_p = dir.join(sub);
        if sub_p.is_dir() {
            if let Some(path) = search_dir_for_cover(&sub_p, true) {
                return Some(path);
            }
        }
    }

    // 3. 检查与音频同名的图片
    if let Some(stem) = audio.file_stem().and_then(|s| s.to_str()) {
        if let Some(entries) = list_dir_images(dir) {
            for ext in COVER_EXTS {
                let target = format!("{stem}.{ext}");
                if let Some(path) = find_case_insensitive(&entries, &target) {
                    return Some(path);
                }
            }
        }
    }

    // 4. 检查上级目录 (处理 CUE 或音频位于子目录的情况)
    if let Some(parent) = dir.parent() {
        let parent_name = parent.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !parent_name.is_empty() && parent_name != "music" && parent_name != "media" && parent_name != "/" {
            if let Some(path) = search_dir_for_cover(parent, false) {
                return Some(path);
            }
        }
    }

    None
}

/// 扫描单个目录寻找最佳匹配的封面图片
fn search_dir_for_cover(dir: &Path, is_subdir: bool) -> Option<PathBuf> {
    let entries = list_dir_images(dir)?;
    if entries.is_empty() {
        return None;
    }

    // 按优先名完全/前缀匹配
    for name in COVER_NAMES {
        for ext in COVER_EXTS {
            let target = format!("{name}.{ext}");
            if let Some(path) = find_case_insensitive(&entries, &target) {
                return Some(path);
            }
        }
    }

    // 模糊包含匹配 (例如 "试音极品01_COVER.jpg")
    for entry in &entries {
        if let Some(stem) = entry.file_stem().and_then(|s| s.to_str()) {
            let stem_lower = stem.to_ascii_lowercase();
            for name in COVER_NAMES {
                if stem_lower.contains(name) {
                    return Some(entry.clone());
                }
            }
        }
    }

    // 若在专用封面子目录(Artwork/Scans等)或目录下仅存在 1 张图片，挑选第一张作为封面
    if is_subdir || entries.len() == 1 {
        return entries.into_iter().next();
    }

    None
}

/// 列出目录下所有支持扩展名的图片文件
fn list_dir_images(dir: &Path) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut images = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if COVER_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            images.push(path);
        }
    }
    Some(images)
}

/// 大小写不敏感匹配文件名
fn find_case_insensitive(entries: &[PathBuf], target: &str) -> Option<PathBuf> {
    let target_lower = target.to_ascii_lowercase();
    entries
        .iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase() == target_lower)
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .join("splayer-folder-cover-tests")
            .join(format!("{}-{}", std::process::id(), unique_counter()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique_counter() -> u64 {
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"fake image").unwrap();
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn finds_cover_jpg() {
        let dir = temp_dir();
        touch(&dir, "cover.jpg");
        touch(&dir, "song.flac");

        let result = find_folder_cover(dir.join("song.flac").to_str().unwrap());
        assert_eq!(
            result.map(|p| p.file_name().unwrap().to_str().unwrap().to_string()),
            Some("cover.jpg".to_string())
        );
        cleanup(&dir);
    }

    #[test]
    fn priority_cover_over_folder() {
        let dir = temp_dir();
        touch(&dir, "cover.png");
        touch(&dir, "folder.jpg");
        touch(&dir, "song.flac");

        let result = find_folder_cover(dir.join("song.flac").to_str().unwrap());
        assert_eq!(
            result.map(|p| p.file_name().unwrap().to_str().unwrap().to_string()),
            Some("cover.png".to_string())
        );
        cleanup(&dir);
    }

    #[test]
    fn same_name_as_audio() {
        let dir = temp_dir();
        touch(&dir, "track01.jpg");
        touch(&dir, "other.txt");
        touch(&dir, "track01.flac");

        let result = find_folder_cover(dir.join("track01.flac").to_str().unwrap());
        assert_eq!(
            result.map(|p| p.file_name().unwrap().to_str().unwrap().to_string()),
            Some("track01.jpg".to_string())
        );
        cleanup(&dir);
    }

    #[test]
    fn single_image_in_dir() {
        let dir = temp_dir();
        touch(&dir, "random_name.jpg");
        touch(&dir, "track.flac");

        let result = find_folder_cover(dir.join("track.flac").to_str().unwrap());
        assert!(result.is_some());
        cleanup(&dir);
    }

    #[test]
    fn multiple_random_images_returns_none() {
        let dir = temp_dir();
        touch(&dir, "a.jpg");
        touch(&dir, "b.png");
        touch(&dir, "track.flac");

        let result = find_folder_cover(dir.join("track.flac").to_str().unwrap());
        assert!(result.is_none());
        cleanup(&dir);
    }

    #[test]
    fn no_images_returns_none() {
        let dir = temp_dir();
        touch(&dir, "track.flac");

        let result = find_folder_cover(dir.join("track.flac").to_str().unwrap());
        assert!(result.is_none());
        cleanup(&dir);
    }

    #[test]
    fn case_insensitive_match() {
        let dir = temp_dir();
        touch(&dir, "Cover.JPG");
        touch(&dir, "song.flac");

        let result = find_folder_cover(dir.join("song.flac").to_str().unwrap());
        assert_eq!(
            result.map(|p| p.file_name().unwrap().to_str().unwrap().to_string()),
            Some("Cover.JPG".to_string())
        );
        cleanup(&dir);
    }

    #[test]
    fn files_without_extension_do_not_break_scan() {
        let dir = temp_dir();
        touch(&dir, "cover.jpg");
        touch(&dir, "README");
        touch(&dir, "song.flac");

        let result = find_folder_cover(dir.join("song.flac").to_str().unwrap());
        assert_eq!(
            result.map(|p| p.file_name().unwrap().to_str().unwrap().to_string()),
            Some("cover.jpg".to_string())
        );
        cleanup(&dir);
    }
}
