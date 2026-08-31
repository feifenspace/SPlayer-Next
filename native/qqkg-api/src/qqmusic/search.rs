//! QQ 音乐四类型搜索与规范化映射。
//!
//! 对齐桌面端 electron/main/apis/qqmusic/modules/search.ts：
//! 上游 DoSearchForQQMusicMobile + 每类型独立信封（{code, total, songs} 等），
//! 字段名为前端 utils/format/qqmusic.ts 消费的驼峰集。

use serde_json::{json, Value};

use super::QqmusicClient;
use crate::error::QqkgError;
use crate::normalize::*;
use crate::types::{SearchParams, SearchType};

const SEARCH_MODULE: &str = "music.search.SearchCgiService";
const SEARCH_METHOD: &str = "DoSearchForQQMusicMobile";

impl QqmusicClient {
    pub async fn search(&self, p: &SearchParams) -> Result<Value, QqkgError> {
        if p.keyword.is_empty() {
            return Ok(json!({ "code": 400, "total": 0, "message": "keywords required" }));
        }
        let param = json!({
            "query": p.keyword,
            "page_num": p.page,
            "num_per_page": p.limit,
            "search_type": p.ty.qq_search_type(),
            "grp": 1,
        });
        let data = self.post_fcg(SEARCH_MODULE, SEARCH_METHOD, param).await?;
        let body = data.get("body").cloned().unwrap_or(Value::Null);
        let sum = data
            .get("meta")
            .and_then(|m| m.get("sum"))
            .and_then(Value::as_u64);

        Ok(match p.ty {
            SearchType::Song => {
                let songs: Vec<Value> = body
                    .get("item_song")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(normalize_song).collect())
                    .unwrap_or_default();
                json!({ "code": 200, "total": sum.unwrap_or(songs.len() as u64), "songs": songs })
            }
            SearchType::Album => {
                let albums: Vec<Value> = body
                    .get("item_album")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(normalize_album).collect())
                    .unwrap_or_default();
                json!({ "code": 200, "total": sum.unwrap_or(albums.len() as u64), "albums": albums })
            }
            SearchType::Artist => {
                let artists: Vec<Value> = body
                    .get("singer")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(normalize_artist).collect())
                    .unwrap_or_default();
                json!({ "code": 200, "total": sum.unwrap_or(artists.len() as u64), "artists": artists })
            }
            SearchType::Playlist => {
                let playlists: Vec<Value> = body
                    .get("item_songlist")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(normalize_playlist).collect())
                    .unwrap_or_default();
                json!({
                    "code": 200,
                    "total": sum.unwrap_or(playlists.len() as u64),
                    "playlists": playlists
                })
            }
        })
    }
}

/// QQ 歌手数组拼接（对齐桌面端 formatSingerName：过滤空名后 " / " 连接）。
fn qq_singer_names(singers: &Value) -> String {
    singers
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("name").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default()
}

pub(crate) fn normalize_song(song: &Value) -> Value {
    let album = song.get("album").cloned().unwrap_or(Value::Null);
    let file = song.get("file").cloned().unwrap_or(Value::Null);
    let pay = song.get("pay").cloned().unwrap_or(Value::Null);


    let album_mid = album.get("mid").and_then(Value::as_str).unwrap_or_default();
    let album_pmid = album.get("pmid").and_then(Value::as_str).unwrap_or_default();
    let picture_mid = if album_mid.is_empty() { album_pmid } else { album_mid };

    // payalbum：非数字专辑（pay_month==0）但定价 > 0 → 1
    let pay_month_is_zero = pay.get("pay_month").and_then(Value::as_i64) == Some(0);
    let price_album = pay.get("price_album").and_then(Value::as_f64).unwrap_or_default();
    let payalbum = if pay_month_is_zero && price_album > 0.0 { 1 } else { 0 };

    // album?.name || album?.title || ""
    let album_name = album
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| album.get("title").and_then(Value::as_str))
        .unwrap_or_default();

    json!({
        "id": val_str_or_num(song, "id").unwrap_or_default(),
        "mid": val_str(song, "mid"),
        "name": val_str(song, "title"),
        "artist": qq_singer_names(&song.get("singer").cloned().unwrap_or(Value::Null)),
        "artists": song.get("singer").cloned().unwrap_or(json!([])),
        "album": album_name,
        "albumMid": album_mid,
        "duration": val_u64(song, "interval") * 1000,
        "mediaMid": val_str(&file, "media_mid"),
        "pay": {
            "payalbum": payalbum,
            "payplay": val_i64(&pay, "pay_play"),
        },
        "size128": val_u64(&file, "size_128mp3"),
        "size320": val_u64(&file, "size_320mp3"),
        "sizeApe": val_u64(&file, "size_ape"),
        "sizeFlac": val_u64(&file, "size_flac"),
        "sizeOgg": val_u64(&file, "size_192ogg"),
        "sizeHiRes": file
            .get("size_new")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        "hiResSampleRate": val_u64(&file, "hires_sample"),
        "hiResBitDepth": val_u64(&file, "hires_bitdepth"),
        "cover": if picture_mid.is_empty() {
            String::new()
        } else {
            format!("https://y.gtimg.cn/music/photo_new/T002R300x300M000{picture_mid}.jpg")
        },
        "coverOriginal": if picture_mid.is_empty() {
            String::new()
        } else {
            format!("https://y.gtimg.cn/music/photo_new/T002R800x800M000{picture_mid}.jpg")
        },
    })
}

fn normalize_album(album: &Value) -> Value {
    let singer_list = album.get("singer_list").cloned().unwrap_or(Value::Null);
    let artist = album
        .get("singer")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| qq_singer_names(&singer_list));
    json!({
        "id": album
            .get("albummid")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| val_str_or_num(album, "id"))
            .unwrap_or_default(),
        "name": val_str(album, "name"),
        "cover": secure_url(&val_str(album, "pic")),
        "artist": artist,
        "artistMid": singer_list
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s.get("mid"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "trackCount": val_u64(album, "song_num"),
    })
}

fn normalize_artist(artist: &Value) -> Value {
    let cover = artist
        .get("singerPic")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| artist.get("iconurl").and_then(Value::as_str))
        .unwrap_or_default();
    json!({
        "id": artist
            .get("singerMID")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| val_str_or_num(artist, "singerID"))
            .unwrap_or_default(),
        "name": val_str(artist, "singerName"),
        "cover": secure_url(cover),
        "albumCount": val_u64(artist, "albumNum"),
        "songCount": val_u64(artist, "songNum"),
    })
}

fn normalize_playlist(pl: &Value) -> Value {
    let cover = pl
        .get("logo")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| pl.get("layer_url").and_then(Value::as_str))
        .unwrap_or_default();
    json!({
        "id": val_str(pl, "dissid"),
        "name": strip_highlight(&val_str(pl, "dissname")),
        "cover": secure_url(cover),
        "creator": val_str(pl, "nickname"),
        "trackCount": val_u64(pl, "songnum"),
        "playCount": val_u64(pl, "listennum"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_song_maps_all_fields() {
        let raw = json!({
            "id": 12345,
            "mid": "001x3MDA1q0fIV",
            "title": "晴天",
            "interval": 269,
            "singer": [
                { "id": 1, "mid": "003Nz2So3XXYek", "name": "周杰伦" },
                { "id": 2, "mid": "002Neh8l0RxIVZ", "name": "" }
            ],
            "album": { "mid": "003Y5iVb1W1KaM", "name": "叶惠美", "title": "fallback", "pmid": "PMID" },
            "file": {
                "media_mid": "media123",
                "size_128mp3": 4310000,
                "size_320mp3": 43000000,
                "size_flac": 28300000,
                "size_new": [56000000],
                "hires_sample": 192000,
                "hires_bitdepth": 24
            },
            "pay": { "pay_month": 0, "pay_play": 1, "price_album": 20 }
        });
        let out = normalize_song(&raw);
        assert_eq!(out["id"], json!("12345"));
        assert_eq!(out["mid"], json!("001x3MDA1q0fIV"));
        assert_eq!(out["name"], json!("晴天"));
        assert_eq!(out["artist"], json!("周杰伦")); // 空名歌手被 filter
        assert_eq!(out["artists"].as_array().unwrap().len(), 2); // artists 原样透传
        assert_eq!(out["album"], json!("叶惠美"));
        assert_eq!(out["albumMid"], json!("003Y5iVb1W1KaM"));
        assert_eq!(out["duration"], json!(269000));
        assert_eq!(out["mediaMid"], json!("media123"));
        assert_eq!(out["pay"]["payalbum"], json!(1)); // pay_month==0 且 price_album>0
        assert_eq!(out["pay"]["payplay"], json!(1));
        assert_eq!(out["size128"], json!(4310000));
        assert_eq!(out["sizeHiRes"], json!(56000000));
        assert_eq!(out["hiResSampleRate"], json!(192000));
        assert_eq!(out["cover"], json!("https://y.gtimg.cn/music/photo_new/T002R300x300M000003Y5iVb1W1KaM.jpg"));
        assert_eq!(out["coverOriginal"], json!("https://y.gtimg.cn/music/photo_new/T002R800x800M000003Y5iVb1W1KaM.jpg"));
    }

    #[test]
    fn normalize_song_falls_back_to_pmid_and_title() {
        let raw = json!({
            "album": { "pmid": "P001", "title": "T-Album" },
            "singer": []
        });
        let out = normalize_song(&raw);
        assert_eq!(out["albumMid"], json!(""));
        assert_eq!(out["cover"], json!("https://y.gtimg.cn/music/photo_new/T002R300x300M000P001.jpg"));
        assert_eq!(out["album"], json!("T-Album")); // name 缺失 → title
        assert_eq!(out["duration"], json!(0));
    }

    #[test]
    fn normalize_album_artist_string_preferred() {
        let raw = json!({
            "albummid": "MID1", "id": 9, "name": "A", "pic": "http://p/c.jpg",
            "singer": "群星", "singer_list": [{ "mid": "SM1", "name": "X" }],
            "song_num": 10
        });
        let out = normalize_album(&raw);
        assert_eq!(out["id"], json!("MID1"));
        assert_eq!(out["artist"], json!("群星"));
        assert_eq!(out["artistMid"], json!("SM1"));
        assert_eq!(out["cover"], json!("https://p/c.jpg"));
        assert_eq!(out["trackCount"], json!(10));
    }

    #[test]
    fn normalize_artist_and_playlist() {
        let a = normalize_artist(&json!({
            "singerMID": "SM9", "singerID": 77, "singerName": "歌手",
            "iconurl": "http://i/a.jpg", "albumNum": 3, "songNum": 88
        }));
        assert_eq!(a["id"], json!("SM9"));
        assert_eq!(a["cover"], json!("https://i/a.jpg"));

        let pl = normalize_playlist(&json!({
            "dissid": "D1", "dissname": "歌<em>单</em>", "logo": "http://l.jpg",
            "nickname": "创建者", "songnum": 5, "listennum": 100
        }));
        assert_eq!(pl["name"], json!("歌单"));
        assert_eq!(pl["playCount"], json!(100));
    }
}
