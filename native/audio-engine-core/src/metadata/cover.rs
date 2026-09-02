use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use ffmpeg_audio::AudioReader;

use super::folder_cover::find_folder_cover;

/// 缩略图最大边长（px）
const THUMB_SIZE: u32 = 300;

const DIRECTORY_THUMB_PREFIX: &str = "directory_cover_";

/// 计算源文件对应的封面缩略图缓存路径（按源路径哈希命名）
pub fn cover_thumb_path(source: &str, cache_dir: &str) -> std::path::PathBuf {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    let hash = hasher.finish();
    Path::new(cache_dir).join(format!("cover_{hash:016x}_thumb.jpg"))
}

/// 复用统一目录封面查找规则，避免 scanner 与单曲 metadata 产生两套优先级。
pub fn find_directory_cover(source: &Path) -> Option<PathBuf> {
    find_folder_cover(source.to_string_lossy().as_ref())
}

/// 同目录封面按图片路径共享一份缩略图，避免同专辑每首曲目重复缓存。
fn directory_cover_thumb_path(source: &Path, cache_dir: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    let hash = hasher.finish();
    Path::new(cache_dir).join(format!("{DIRECTORY_THUMB_PREFIX}{hash:016x}_thumb.jpg"))
}

fn source_is_newer(source: &Path, cached: &Path) -> bool {
    let Ok(source_modified) = source.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let Ok(cached_modified) = cached.metadata().and_then(|metadata| metadata.modified()) else {
        return true;
    };
    source_modified > cached_modified
}

pub fn extract_directory_cover_thumbnail(source: &Path, cache_dir: &str) -> Option<String> {
    let thumb_file = directory_cover_thumb_path(source, cache_dir);
    if thumb_file.exists() && !source_is_newer(source, &thumb_file) {
        return Some(thumb_file.to_string_lossy().into_owned());
    }

    let cover = fs::read(source).ok()?;
    fs::create_dir_all(cache_dir).ok()?;
    generate_cover_thumbnail(&cover, &thumb_file).ok()?;
    Some(thumb_file.to_string_lossy().into_owned())
}

/// 判断未变化的音频是否需要因为封面缓存状态而重新探测。
pub fn cover_cache_needs_refresh(
    cached_cover: Option<&str>,
    cache_dir: &str,
    directory_cover: Option<&Path>,
) -> bool {
    let Some(cached_cover) = cached_cover else {
        return directory_cover.is_some();
    };
    let cached_path = Path::new(cached_cover);
    if !cached_path.exists() {
        return true;
    }
    if !cached_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(DIRECTORY_THUMB_PREFIX))
    {
        return false;
    }
    let Some(directory_cover) = directory_cover else {
        return true;
    };
    cached_path != directory_cover_thumb_path(directory_cover, cache_dir)
        || source_is_newer(directory_cover, cached_path)
}

/// 从 reader 中提取封面缩略图，写入缓存目录，返回缩略图路径。
pub fn extract_cover_thumbnail_with_directory_cover(
    reader: &AudioReader,
    source: &str,
    cache_dir: &str,
    directory_cover: Option<&Path>,
) -> Option<String> {
    let thumb_file = cover_thumb_path(source, cache_dir);

    if thumb_file.exists() {
        return Some(thumb_file.to_string_lossy().into_owned());
    }

    if let Some(cover) = reader.cover() {
        fs::create_dir_all(cache_dir).ok()?;
        generate_cover_thumbnail(&cover.data, &thumb_file).ok()?;
        return Some(thumb_file.to_string_lossy().into_owned());
    }

    let directory_cover = directory_cover?;
    extract_directory_cover_thumbnail(&directory_cover, cache_dir)
}

/// 提取内嵌封面；没有内嵌图时查找同目录约定封面。
pub fn extract_cover_thumbnail(
    reader: &AudioReader,
    source: &str,
    cache_dir: &str,
) -> Option<String> {
    let directory_cover = find_directory_cover(Path::new(source));
    extract_cover_thumbnail_with_directory_cover(
        reader,
        source,
        cache_dir,
        directory_cover.as_deref(),
    )
}

/// 从指定文件所在目录提取外部封面图片并生成缩略图
pub fn extract_folder_cover_thumbnail(source: &str, cache_dir: &str) -> Option<String> {
    let directory_cover = find_directory_cover(Path::new(source))?;
    extract_directory_cover_thumbnail(&directory_cover, cache_dir)
}

/// 拿原始封面字节（供 SMTC / 全屏播放器使用，不缓存）
pub fn read_attached_pic(reader: &AudioReader) -> Option<Vec<u8>> {
    reader.cover().map(|cover| cover.data)
}

/// 将任意图片字节缩放为 JPEG 缩略图字节（内存内，不落盘）。
/// 用于选图预览：原生层缩好再交给渲染层，避免渲染层把整图解码成位图占内存
pub fn make_thumbnail_jpeg(data: &[u8], max_size: u32) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory(data)?;
    let thumbnail = image.thumbnail(max_size, max_size);
    let mut output = Vec::new();
    thumbnail.write_to(&mut Cursor::new(&mut output), image::ImageFormat::Jpeg)?;
    Ok(output)
}

/// 将原始图片数据缩放为 JPEG 缩略图
fn generate_cover_thumbnail(
    data: &[u8],
    output_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let image = image::load_from_memory(data)?;
    let thumbnail = image.thumbnail(THUMB_SIZE, THUMB_SIZE);
    thumbnail.save_with_format(output_path, image::ImageFormat::Jpeg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_output_path(name: &str) -> std::path::PathBuf {
        let sequence = TEST_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "splayer-metadata-{}-{sequence}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn invalid_cover_does_not_create_fake_jpeg() {
        let output = test_output_path("invalid.jpg");
        let _ = std::fs::remove_file(&output);

        assert!(generate_cover_thumbnail(b"not an image", &output).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn png_cover_is_encoded_as_jpeg() {
        let output = test_output_path("converted.jpg");
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();

        generate_cover_thumbnail(png.get_ref(), &output).unwrap();

        let cached = std::fs::read(&output).unwrap();
        assert!(cached.starts_with(&[0xff, 0xd8, 0xff]));
        let _ = std::fs::remove_file(output);
    }

    #[test]
    fn directory_cover_lookup_is_case_insensitive_and_uses_stable_priority() {
        let root = test_output_path("directory-cover-priority");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Folder.JPG"), b"folder").unwrap();
        fs::write(root.join("cover.png"), b"cover").unwrap();

        assert_eq!(
            find_directory_cover(&root.join("track.flac")),
            Some(root.join("cover.png")),
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn directory_cover_cache_is_shared_and_repaired_when_missing() {
        let root = test_output_path("directory-cover-cache");
        let cache = root.join("cache");
        fs::create_dir_all(&root).unwrap();
        let cover = root.join("cover.png");
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        fs::write(&cover, png.get_ref()).unwrap();

        let cached = extract_directory_cover_thumbnail(&cover, cache.to_str().unwrap()).unwrap();
        assert!(Path::new(&cached).exists());
        assert_eq!(
            cached,
            extract_directory_cover_thumbnail(&cover, cache.to_str().unwrap()).unwrap(),
        );
        assert!(!cover_cache_needs_refresh(
            Some(&cached),
            cache.to_str().unwrap(),
            Some(&cover),
        ));
        fs::remove_file(&cached).unwrap();
        assert!(cover_cache_needs_refresh(
            Some(&cached),
            cache.to_str().unwrap(),
            Some(&cover),
        ));
        let _ = fs::remove_dir_all(root);
    }
}
