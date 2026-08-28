use audio_engine_core::cue::{
    decode_text_auto, is_cue_path, normalize_for_fuzzy_match, parse_cue_virtual_path,
    parse_timestamp_frames, resolve_audio_path, CueSheet,
};
use std::fs::File;

#[test]
fn test_timestamp_parser() {
    // 00:00:00 -> 0
    assert_eq!(parse_timestamp_frames("00:00:00"), Some(0));
    // 01:23:45 -> (1*60 + 23)*75 + 45 = 83*75 + 45 = 6225 + 45 = 6270
    assert_eq!(parse_timestamp_frames("01:23:45"), Some(6270));
    // Invalid
    assert_eq!(parse_timestamp_frames("invalid"), None);
}

#[test]
fn test_fuzzy_matching() {
    let temp_dir = tempfile::tempdir().unwrap();
    let dir = temp_dir.path();

    // 1. 测试大小写不敏感
    let audio_file = dir.join("Album_Track_01.flac");
    File::create(&audio_file).unwrap();

    let resolved = resolve_audio_path(dir, "album_track_01.flac");
    assert_eq!(resolved, audio_file);

    // 2. 测试忽略空格与标点
    let resolved_fuzzy = resolve_audio_path(dir, "Album - Track 01.flac");
    assert_eq!(resolved_fuzzy, audio_file);

    assert_eq!(
        normalize_for_fuzzy_match("01. Artist - Song (Remix).flac"),
        "01artistsongremixflac"
    );
}

#[test]
fn test_cue_sheet_utf8_parsing() {
    let temp_dir = tempfile::tempdir().unwrap();
    let dir = temp_dir.path();

    let flac_file = dir.join("album.flac");
    File::create(&flac_file).unwrap();

    let cue_content = r#"
REM GENRE "Progressive Rock"
REM DATE 1973
PERFORMER "Pink Floyd"
TITLE "The Dark Side of the Moon"
FILE "album.flac" WAVE
  TRACK 01 AUDIO
    TITLE "Speak to Me"
    PERFORMER "Pink Floyd"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Breathe"
    PERFORMER "Pink Floyd"
    INDEX 00 01:05:00
    INDEX 01 01:07:00
  TRACK 03 AUDIO
    TITLE "On the Run"
    PERFORMER "Pink Floyd"
    INDEX 01 03:50:00
"#;

    let cue_file = dir.join("album.cue");
    std::fs::write(&cue_file, cue_content).unwrap();

    let cue = CueSheet::parse_file(&cue_file).unwrap();
    assert_eq!(
        cue.global_title.as_deref(),
        Some("The Dark Side of the Moon")
    );
    assert_eq!(cue.global_performer.as_deref(), Some("Pink Floyd"));
    assert_eq!(cue.tracks.len(), 3);

    // Track 1
    assert_eq!(cue.tracks[0].track_num, 1);
    assert_eq!(cue.tracks[0].title.as_deref(), Some("Speak to Me"));
    assert_eq!(cue.tracks[0].cue_start_frames, 0);
    assert_eq!(cue.tracks[0].start_time, 0.0);
    assert_eq!(cue.tracks[0].cue_duration_frames, Some(5025)); // 01:07:00 = 67*75 = 5025
    assert_eq!(cue.tracks[0].duration, Some(67.0));

    // Track 2
    assert_eq!(cue.tracks[1].track_num, 2);
    assert_eq!(cue.tracks[1].title.as_deref(), Some("Breathe"));
    assert_eq!(cue.tracks[1].index0_frames, Some(4875)); // 01:05:00 = 65*75 = 4875
    assert_eq!(cue.tracks[1].cue_start_frames, 5025);
    assert_eq!(cue.tracks[1].start_time, 67.0);
    // 03:50:00 = 230*75 = 17250 -> duration = 17250 - 5025 = 12225 (163s)
    assert_eq!(cue.tracks[1].cue_duration_frames, Some(12225));
    assert_eq!(cue.tracks[1].duration, Some(163.0));

    // Track 3
    assert_eq!(cue.tracks[2].track_num, 3);
    assert_eq!(cue.tracks[2].title.as_deref(), Some("On the Run"));
    assert_eq!(cue.tracks[2].start_time, 230.0);
    assert_eq!(cue.tracks[2].duration, None); // 最后一轨

    // 测试虚拟路径解析
    let virt = &cue.tracks[0].virtual_path;
    let parsed_virt = parse_cue_virtual_path(virt).unwrap();
    assert_eq!(parsed_virt.physical_path, flac_file.to_string_lossy());
    assert_eq!(parsed_virt.start_time, 0.0);
    assert_eq!(parsed_virt.duration, 67.0);
    assert_eq!(parsed_virt.track_num, 1);
}

#[test]
fn test_cue_sheet_gbk_encoding_detection() {
    use encoding_rs::GB18030;

    let temp_dir = tempfile::tempdir().unwrap();
    let dir = temp_dir.path();

    let gbk_cue_text = r#"
PERFORMER "张学友"
TITLE "真情年代"
FILE "jacky.wav" WAVE
  TRACK 01 AUDIO
    TITLE "吻别"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "一千个伤心的理由"
    INDEX 01 04:30:00
"#;

    let (gbk_bytes, _, _) = GB18030.encode(gbk_cue_text);
    let decoded = decode_text_auto(&gbk_bytes);
    assert!(decoded.contains("张学友"));
    assert!(decoded.contains("吻别"));
    assert!(decoded.contains("一千个伤心的理由"));

    let cue_file = dir.join("gbk_test.cue");
    std::fs::write(&cue_file, gbk_bytes).unwrap();

    let cue = CueSheet::parse_file(&cue_file).unwrap();
    assert_eq!(cue.global_performer.as_deref(), Some("张学友"));
    assert_eq!(cue.global_title.as_deref(), Some("真情年代"));
    assert_eq!(cue.tracks[0].title.as_deref(), Some("吻别"));
    assert_eq!(cue.tracks[1].title.as_deref(), Some("一千个伤心的理由"));
    assert!(is_cue_path("test.CUE"));
}
