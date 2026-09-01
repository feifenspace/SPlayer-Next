#![cfg(feature = "napi")]

//! 集成测试：扫描器核心 API
//!
//! 这些测试验证 NAPI wrapper 跨 crate 使用扫描器时的公共接口。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use audio_engine_core::scanner::{FileRecord, ScanEvent, ScannedTrack};

/// 收集 scan_directories 回调事件的辅助结构
#[derive(Default)]
struct EventSink(Arc<Mutex<Vec<ScanEvent>>>);

impl EventSink {
    fn take_events(&self) -> Vec<ScanEvent> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// 创建一个唯一的空目录（测试结束后清理）
fn unique_temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("audio-engine-core-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

#[test]
fn scan_empty_directory_produces_done_event() {
    let dir = unique_temp_dir("empty");
    let sink = EventSink::default();
    let callback = |event: ScanEvent| sink.0.lock().unwrap().push(event);
    let cancel = AtomicBool::new(false);

    audio_engine_core::scanner::scan_directories(
        &[dir.to_string_lossy().into_owned()],
        None,
        None,
        &cancel,
        &callback,
    );

    let _ = fs::remove_dir_all(&dir);

    let events = sink.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ScanEvent::Done {
            scanned,
            total,
            removed_paths,
            cue_files,
            iso_files,
            unavailable_dirs,
        } => {
            assert_eq!(*scanned, 0);
            assert_eq!(*total, 0);
            assert!(removed_paths.is_empty());
            assert!(cue_files.is_empty());
            assert!(iso_files.is_empty());
            assert!(unavailable_dirs.is_empty());
        }
        ScanEvent::Progress { .. } => panic!("unexpected Progress event"),
    }
}

#[test]
fn scan_nonexistent_directory_reports_unavailable() {
    let sink = EventSink::default();
    let callback = |event: ScanEvent| sink.0.lock().unwrap().push(event);
    let cancel = AtomicBool::new(false);

    audio_engine_core::scanner::scan_directories(
        &["/no/such/audio-engine-core-directory".to_string()],
        None,
        None,
        &cancel,
        &callback,
    );

    let events = sink.take_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ScanEvent::Done {
            scanned,
            total,
            unavailable_dirs,
            ..
        } => {
            assert_eq!(*scanned, 0);
            assert_eq!(*total, 0);
            assert_eq!(unavailable_dirs.len(), 1);
            assert!(unavailable_dirs[0].contains("no/such/audio-engine-core-directory"));
        }
        ScanEvent::Progress { .. } => panic!("unexpected Progress event"),
    }
}

#[test]
fn scan_cancelled_during_walk_phase_stops_early() {
    let dir = unique_temp_dir("cancelled");
    let sink = EventSink::default();
    let callback = |event: ScanEvent| sink.0.lock().unwrap().push(event);
    let cancel = AtomicBool::new(true);

    audio_engine_core::scanner::scan_directories(
        &[dir.to_string_lossy().into_owned()],
        None,
        None,
        &cancel,
        &callback,
    );

    let _ = fs::remove_dir_all(&dir);

    // 取消发生在文件收集阶段，直接返回且不发任何事件
    assert!(sink.take_events().is_empty());
}

#[test]
fn file_record_round_trips_path_mtime_size() {
    let record = FileRecord {
        path: "tests/fixtures/track.mp3".to_string(),
        mtime: 1_000_000,
        size: 2_048,
    };

    assert_eq!(record.path, "tests/fixtures/track.mp3");
    assert_eq!(record.mtime, 1_000_000);
    assert_eq!(record.size, 2_048);
}

#[test]
fn scanned_track_holds_all_expected_fields() {
    let track = ScannedTrack {
        path: "tests/fixtures/track.mp3".to_string(),
        title: Some("Title".to_string()),
        artist: Some("Artist".to_string()),
        album: Some("Album".to_string()),
        track: Some(1),
        duration: 180.5,
        codec: "mp3".to_string(),
        sample_rate: 44_100,
        bit_rate: 128_000,
        channels: 2,
        bits_per_sample: 16,
        cover: Some("tests/cache/cover.jpg".to_string()),
        file_size: 4_096,
        mtime: 1_000_000,
        ctime: 500_000,
    };

    assert_eq!(track.title, Some("Title".to_string()));
    assert_eq!(track.artist, Some("Artist".to_string()));
    assert_eq!(track.album, Some("Album".to_string()));
    assert_eq!(track.track, Some(1));
    assert!((track.duration - 180.5).abs() < f64::EPSILON);
    assert_eq!(track.codec, "mp3");
    assert_eq!(track.sample_rate, 44_100);
    assert_eq!(track.bit_rate, 128_000);
    assert_eq!(track.channels, 2);
    assert_eq!(track.bits_per_sample, 16);
    assert_eq!(track.cover, Some("tests/cache/cover.jpg".to_string()));
    assert_eq!(track.file_size, 4_096);
    assert_eq!(track.mtime, 1_000_000);
    assert_eq!(track.ctime, 500_000);
}
