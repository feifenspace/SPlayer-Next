use audio_engine_core::sacd::{
    is_sacd_iso_path, parse_sacd_virtual_path, probe_sacd_iso, FrameFormat, IsoReader,
    SACD_LSN_SIZE,
};
use std::fs::File;
use std::io::Write;

#[test]
fn test_sacd_virtual_path_parsing() {
    let path = "/music/album.iso|Track03|245.500|1500|18412|500|1000";
    let info = parse_sacd_virtual_path(path).unwrap();
    assert_eq!(info.iso_path, "/music/album.iso");
    assert_eq!(info.track_num, 3);
    assert_eq!(info.duration_secs, 245.5);
    assert_eq!(info.start_frames, 1500);
    assert_eq!(info.duration_frames, 18412);
    assert_eq!(info.start_lsn, 500);
    assert_eq!(info.length_lsn, 1000);
    assert!(is_sacd_iso_path("/music/album.ISO"));
    assert!(!is_sacd_iso_path("/music/album.flac"));
}

#[test]
fn test_synthetic_sacd_iso_parsing() {
    let temp_dir = tempfile::tempdir().unwrap();
    let iso_path = temp_dir.path().join("The Beatles - Abbey Road.iso");

    let total_sectors = 600usize;
    let mut iso_bytes = vec![0u8; total_sectors * SACD_LSN_SIZE];

    // 1. Sector 510: Master TOC
    let master_toc_off = 510 * SACD_LSN_SIZE;
    iso_bytes[master_toc_off..master_toc_off + 8].copy_from_slice(b"SACDMTOC");
    iso_bytes[master_toc_off + 64..master_toc_off + 68].copy_from_slice(&520u32.to_be_bytes());
    iso_bytes[master_toc_off + 84..master_toc_off + 86].copy_from_slice(&10u16.to_be_bytes());

    // 2. Sector 511: Master Text
    let master_text_off = 511 * SACD_LSN_SIZE;
    iso_bytes[master_text_off..master_text_off + 8].copy_from_slice(b"SACDText");
    // album_title_position = 48, album_artist_position = 68
    iso_bytes[master_text_off + 16..master_text_off + 18].copy_from_slice(&48u16.to_be_bytes());
    iso_bytes[master_text_off + 18..master_text_off + 20].copy_from_slice(&68u16.to_be_bytes());
    let title_bytes = b"Abbey Road\0";
    let artist_bytes = b"The Beatles\0";
    iso_bytes[master_text_off + 48..master_text_off + 48 + title_bytes.len()]
        .copy_from_slice(title_bytes);
    iso_bytes[master_text_off + 68..master_text_off + 68 + artist_bytes.len()]
        .copy_from_slice(artist_bytes);

    // 3. Sector 520: Area 1 TOC (TWOCHTOC)
    let area_toc_off = 520 * SACD_LSN_SIZE;
    iso_bytes[area_toc_off..area_toc_off + 8].copy_from_slice(b"TWOCHTOC");
    iso_bytes[area_toc_off + 21] = 0x00; // frame_format = DST
    iso_bytes[area_toc_off + 32] = 2; // channel_count = 2
    iso_bytes[area_toc_off + 69] = 2; // track_count = 2

    // 4. Sector 521: SACDTRL1 (Track start & length LSN)
    let trl1_off = 521 * SACD_LSN_SIZE;
    iso_bytes[trl1_off..trl1_off + 8].copy_from_slice(b"SACDTRL1");
    iso_bytes[trl1_off + 8..trl1_off + 12].copy_from_slice(&550u32.to_be_bytes());
    iso_bytes[trl1_off + 12..trl1_off + 16].copy_from_slice(&570u32.to_be_bytes());
    let len_base = trl1_off + 8 + 255 * 4;
    iso_bytes[len_base..len_base + 4].copy_from_slice(&20u32.to_be_bytes());
    iso_bytes[len_base + 4..len_base + 8].copy_from_slice(&25u32.to_be_bytes());

    // 5. Sector 522: SACDTRL2 (Track timestamps)
    let trl2_off = 522 * SACD_LSN_SIZE;
    iso_bytes[trl2_off..trl2_off + 8].copy_from_slice(b"SACDTRL2");
    iso_bytes[trl2_off + 8] = 0;
    iso_bytes[trl2_off + 9] = 0;
    iso_bytes[trl2_off + 10] = 0;
    iso_bytes[trl2_off + 12] = 4;
    iso_bytes[trl2_off + 13] = 20;
    iso_bytes[trl2_off + 14] = 0;

    let dur_base = trl2_off + 8 + 255 * 4;
    iso_bytes[dur_base] = 4;
    iso_bytes[dur_base + 1] = 20;
    iso_bytes[dur_base + 2] = 0;
    iso_bytes[dur_base + 4] = 3;
    iso_bytes[dur_base + 5] = 3;
    iso_bytes[dur_base + 6] = 0;

    // 6. Sector 523: SACDTTxt
    let ttxt_off = 523 * SACD_LSN_SIZE;
    iso_bytes[ttxt_off..ttxt_off + 8].copy_from_slice(b"SACDTTxt");
    // track_text_position[0] = 0 (area text), [1] = 30 (Track 1), [2] = 60 (Track 2)
    iso_bytes[ttxt_off + 10..ttxt_off + 12].copy_from_slice(&30u16.to_be_bytes());
    iso_bytes[ttxt_off + 12..ttxt_off + 14].copy_from_slice(&60u16.to_be_bytes());

    // Track 1: Type 0x01 (Title) "Come Together" + Type 0x02 (Performer) "The Beatles"
    iso_bytes[ttxt_off + 30] = 0x01;
    let t1_title = b"Come Together\0";
    iso_bytes[ttxt_off + 31..ttxt_off + 31 + t1_title.len()].copy_from_slice(t1_title);
    let p2 = 31 + t1_title.len();
    iso_bytes[ttxt_off + p2] = 0x02;
    let t1_artist = b"The Beatles\0";
    iso_bytes[ttxt_off + p2 + 1..ttxt_off + p2 + 1 + t1_artist.len()].copy_from_slice(t1_artist);

    // Track 2: Type 0x01 (Title) "Something" + Type 0x02 (Performer) "The Beatles"
    iso_bytes[ttxt_off + 60] = 0x01;
    let t2_title = b"Something\0";
    iso_bytes[ttxt_off + 61..ttxt_off + 61 + t2_title.len()].copy_from_slice(t2_title);
    let p3 = 61 + t2_title.len();
    iso_bytes[ttxt_off + p3] = 0x02;
    let t2_artist = b"The Beatles\0";
    iso_bytes[ttxt_off + p3 + 1..ttxt_off + p3 + 1 + t2_artist.len()].copy_from_slice(t2_artist);



    let mut file = File::create(&iso_path).unwrap();
    file.write_all(&iso_bytes).unwrap();
    file.flush().unwrap();

    let reader = IsoReader::open(&iso_path).unwrap();
    assert_eq!(reader.total_lsn(), 600);


    let disc = probe_sacd_iso(&iso_path).unwrap();
    assert_eq!(disc.album_title.as_deref(), Some("Abbey Road"));
    assert_eq!(disc.album_artist.as_deref(), Some("The Beatles"));
    assert_eq!(disc.channel_count, 2);
    assert_eq!(disc.frame_format, FrameFormat::Dst);
    assert_eq!(disc.tracks.len(), 2);

    let t1 = &disc.tracks[0];
    assert_eq!(t1.track_num, 1);
    assert_eq!(t1.title.as_deref(), Some("Come Together"));
    assert_eq!(t1.artist.as_deref(), Some("The Beatles"));
    assert_eq!(t1.duration_secs, 260.0);

    let t2 = &disc.tracks[1];
    assert_eq!(t2.track_num, 2);
    assert_eq!(t2.title.as_deref(), Some("Something"));
    assert_eq!(t2.artist.as_deref(), Some("The Beatles"));
    assert_eq!(t2.duration_secs, 183.0);
}
