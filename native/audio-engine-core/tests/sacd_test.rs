use audio_engine_core::sacd::{
    is_sacd_iso_path, parse_sacd_virtual_path, ArithmeticDecoder, IsoReader, SacdDisc,
    SACD_SECTOR_SIZE,
};
use std::fs::File;
use std::io::Write;

#[test]
fn test_sacd_virtual_path_parsing() {
    let path = "/music/album.iso|Track03|245.500|1500|18412";
    let info = parse_sacd_virtual_path(path).unwrap();
    assert_eq!(info.iso_path, "/music/album.iso");
    assert_eq!(info.track_num, 3);
    assert_eq!(info.duration, 245.5);
    assert_eq!(info.start_frame, 1500);
    assert_eq!(info.duration_frame, 18412);
    assert!(is_sacd_iso_path("/music/album.ISO"));
    assert!(!is_sacd_iso_path("/music/album.flac"));
}

#[test]
fn test_dst_arithmetic_decoder() {
    // 构造算术编码测试数据
    let test_data = vec![0x80, 0x00, 0x55, 0xAA];
    let mut ac = ArithmeticDecoder::new(&test_data);
    let bit1 = ac.decode_bit(128);
    let _bit2 = ac.decode_bit(128);
    assert!(bit1 <= 1);
}

#[test]
fn test_synthetic_sacd_iso_parsing() {
    let temp_dir = tempfile::tempdir().unwrap();
    let iso_path = temp_dir.path().join("The Beatles - Abbey Road.iso");

    // 构造 600 个扇区 (1.2MB) 的 Synthetic SACD ISO
    let total_sectors = 600usize;
    let mut iso_bytes = vec![0u8; total_sectors * SACD_SECTOR_SIZE];

    // 1. Sector 510: Master TOC
    let master_toc_off = 510 * SACD_SECTOR_SIZE;
    iso_bytes[master_toc_off..master_toc_off + 8].copy_from_slice(b"SACDMTOC");
    // area_1_toc_1_start at byte offset 32 (Big Endian)
    iso_bytes[master_toc_off + 32..master_toc_off + 36].copy_from_slice(&520u32.to_be_bytes());
    // area_1_toc_size at byte offset 46 (Big Endian)
    iso_bytes[master_toc_off + 46..master_toc_off + 48].copy_from_slice(&10u16.to_be_bytes());

    // 2. Sector 511: Master Text
    let master_text_off = 511 * SACD_SECTOR_SIZE;
    iso_bytes[master_text_off..master_text_off + 8].copy_from_slice(b"SACDText");
    // album_title_position = 40, album_artist_position = 60
    iso_bytes[master_text_off + 16..master_text_off + 18].copy_from_slice(&40u16.to_be_bytes());
    iso_bytes[master_text_off + 18..master_text_off + 20].copy_from_slice(&60u16.to_be_bytes());
    let title_bytes = b"Abbey Road\0";
    let artist_bytes = b"The Beatles\0";
    iso_bytes[master_text_off + 40..master_text_off + 40 + title_bytes.len()]
        .copy_from_slice(title_bytes);
    iso_bytes[master_text_off + 60..master_text_off + 60 + artist_bytes.len()]
        .copy_from_slice(artist_bytes);

    // 3. Sector 520: Area 1 TOC (TWOCHTOC)
    let area_toc_off = 520 * SACD_SECTOR_SIZE;
    iso_bytes[area_toc_off..area_toc_off + 8].copy_from_slice(b"TWOCHTOC");
    iso_bytes[area_toc_off + 13] = 0x00; // frame_format = DST
    iso_bytes[area_toc_off + 24] = 2; // channel_count = 2
    iso_bytes[area_toc_off + 61] = 2; // track_count = 2

    // 4. Sector 521: SACDTRL1 (Track start & length LSN)
    let trl1_off = 521 * SACD_SECTOR_SIZE;
    iso_bytes[trl1_off..trl1_off + 8].copy_from_slice(b"SACDTRL1");
    // Track 1: start LSN 550, length 20
    iso_bytes[trl1_off + 8..trl1_off + 12].copy_from_slice(&550u32.to_be_bytes());
    iso_bytes[trl1_off + 12..trl1_off + 16].copy_from_slice(&20u32.to_be_bytes());
    // Track 2: start LSN 570, length 25
    iso_bytes[trl1_off + 16..trl1_off + 20].copy_from_slice(&570u32.to_be_bytes());
    iso_bytes[trl1_off + 20..trl1_off + 24].copy_from_slice(&25u32.to_be_bytes());

    // 5. Sector 522: SACDTRL2 (Track timestamps)
    let trl2_off = 522 * SACD_SECTOR_SIZE;
    iso_bytes[trl2_off..trl2_off + 8].copy_from_slice(b"SACDTRL2");
    // Track 1 start: 00:00:00 (offset 8)
    iso_bytes[trl2_off + 8] = 0;
    iso_bytes[trl2_off + 9] = 0;
    iso_bytes[trl2_off + 10] = 0;
    // Track 2 start: 04:20:00 = 260s * 75 = 19500 frames (offset 12)
    iso_bytes[trl2_off + 12] = 4;
    iso_bytes[trl2_off + 13] = 20;
    iso_bytes[trl2_off + 14] = 0;

    // Track 1 dur: 04:20:00 (dur_base = 8 + 255*4 = 1028)
    let dur_base = trl2_off + 8 + 255 * 4;
    iso_bytes[dur_base] = 4;
    iso_bytes[dur_base + 1] = 20;
    iso_bytes[dur_base + 2] = 0;
    // Track 2 dur: 03:03:00 = 183s
    iso_bytes[dur_base + 4] = 3;
    iso_bytes[dur_base + 5] = 3;
    iso_bytes[dur_base + 6] = 0;

    // 6. Sector 523: SACDTTxt
    let ttxt_off = 523 * SACD_SECTOR_SIZE;
    iso_bytes[ttxt_off..ttxt_off + 8].copy_from_slice(b"SACDTTxt");
    // Track 1 text position: 30, Track 2 text position: 60
    iso_bytes[ttxt_off + 8..ttxt_off + 10].copy_from_slice(&30u16.to_be_bytes());
    iso_bytes[ttxt_off + 10..ttxt_off + 12].copy_from_slice(&60u16.to_be_bytes());

    // Track 1 text: 1 item, type 0x01 (Title), text "Come Together\0"
    iso_bytes[ttxt_off + 30] = 1; // 1 item
    iso_bytes[ttxt_off + 34] = 0x01; // Title
    let t1_name = b"Come Together\0";
    iso_bytes[ttxt_off + 36..ttxt_off + 36 + t1_name.len()].copy_from_slice(t1_name);

    // Track 2 text: 1 item, type 0x01 (Title), text "Something\0"
    iso_bytes[ttxt_off + 60] = 1;
    iso_bytes[ttxt_off + 64] = 0x01;
    let t2_name = b"Something\0";
    iso_bytes[ttxt_off + 66..ttxt_off + 66 + t2_name.len()].copy_from_slice(t2_name);

    let mut file = File::create(&iso_path).unwrap();
    file.write_all(&iso_bytes).unwrap();
    file.flush().unwrap();

    // 7. 解析 Synthetic SACD ISO
    let mut reader = IsoReader::open(&iso_path).unwrap();
    assert_eq!(reader.total_sectors, 600);

    let disc = SacdDisc::parse(&mut reader).unwrap();
    assert_eq!(disc.album_title.as_deref(), Some("Abbey Road"));
    assert_eq!(disc.album_artist.as_deref(), Some("The Beatles"));
    assert_eq!(disc.areas.len(), 1);

    let stereo_area = &disc.areas[0];
    assert_eq!(stereo_area.track_count, 2);
    assert_eq!(stereo_area.channel_count, 2);
    assert!(stereo_area.is_dst);

    // 校验 Track 1
    let t1 = &stereo_area.tracks[0];
    assert_eq!(t1.track_num, 1);
    assert_eq!(t1.title.as_deref(), Some("Come Together"));
    assert_eq!(t1.artist.as_deref(), Some("The Beatles"));
    assert_eq!(t1.album.as_deref(), Some("Abbey Road"));
    assert_eq!(t1.start_time, 0.0);
    assert_eq!(t1.duration, 260.0);
    assert_eq!(t1.channels, 2);
    assert_eq!(t1.sample_rate, 2_822_400);

    // 校验 Track 2
    let t2 = &stereo_area.tracks[1];
    assert_eq!(t2.track_num, 2);
    assert_eq!(t2.title.as_deref(), Some("Something"));
    assert_eq!(t2.artist.as_deref(), Some("The Beatles"));
    assert_eq!(t2.start_time, 260.0);
    assert_eq!(t2.duration, 183.0);
}
