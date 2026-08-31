//! 酷狗四类型搜索与规范化映射。
//!
//! 对齐桌面端 electron/main/apis/kugou/modules/search.ts：
//! - 单曲：mobilecdn v3（含 group 展开/去重）→ 空结果时兜底 songsearch legacy
//! - 专辑/歌手/歌单：网关 complexsearch（依赖 M0 的 Android 签名）

use serde_json::{json, Map, Value};

use super::KugouClient;
use crate::error::QqkgError;
use crate::normalize::*;
use crate::types::{SearchParams, SearchType};

const KG_MOBILECDN_URL: &str = "http://mobilecdn.kugou.com/api/v3/search/song";
const KG_LEGACY_SEARCH_URL: &str = "https://songsearch.kugou.com/song_search_v2";
const KG_X_ROUTER: &str = "x-router";
const COMPLEXSEARCH: &str = "complexsearch.kugou.com";

/// 酷狗歌手串格式化（对齐桌面端 formatMobileArtist：按 、,;/ 切分再 " / " 连接）。
fn format_mobile_artist(name: &str) -> String {
    decode_name(name)
        .split(['、', ',', ';', '/'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// f64 → JSON 数值：整数值输出整数（对齐桌面端 JSON.stringify 的数字形态）。
fn num_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

/// 档位 hash/size 提取（JS truthy：非空串、非零数）。
fn quality_hashes(raw: &Value) -> (Vec<String>, Map<String, Value>, Map<String, Value>) {
    let mut qualities = Vec::new();
    let mut hashes = Map::new();
    let mut sizes = Map::new();

    let mut push = |key: &str, hash: Option<String>, size: Option<f64>| {
        if let (Some(h), Some(s)) = (hash, size) {
            qualities.push(key.to_string());
            hashes.insert(key.to_string(), json!(h));
            sizes.insert(key.to_string(), num_json(s));
        }
    };

    push(
        "128k",
        val_str_truthy(raw, "hash"),
        val_num_truthy(raw, "filesize"),
    );
    push(
        "320k",
        raw.get("320hash").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
        val_num_truthy(raw, "320filesize"),
    );
    push(
        "flac",
        val_str_truthy(raw, "sqhash"),
        val_num_truthy(raw, "sqfilesize"),
    );
    let hr_size = raw
        .get("hires_filesize")
        .and_then(Value::as_f64)
        .filter(|n| *n != 0.0)
        .or_else(|| raw.get("resfilesize").and_then(Value::as_f64).filter(|n| *n != 0.0));
    let hr_hash = val_str_truthy(raw, "hires_hash")
        .or_else(|| val_str_truthy(raw, "reshash"));
    push("flac24bit", hr_hash, hr_size);

    (qualities, hashes, sizes)
}

pub(crate) fn normalize_from_mobile(raw: &Value) -> Value {
    let (qualities, hashes, sizes) = quality_hashes(raw);


    let artist_name = format_mobile_artist(&val_str(raw, "singername"));
    let artists: Vec<Value> = artist_name
        .split(" / ")
        .map(|n| json!({ "id": n, "name": n }))
        .collect();

    let id = {
        let audio_id = raw.get("audio_id").and_then(Value::as_u64).unwrap_or_default();
        if audio_id != 0 {
            audio_id.to_string()
        } else {
            val_str_truthy(raw, "hash").unwrap_or_default()
        }
    };

    let name = val_str_truthy(raw, "songname")
        .or_else(|| val_str_truthy(raw, "filename"))
        .map(|s| decode_name(&s))
        .unwrap_or_default();

    let cover_tpl = raw
        .get("trans_param")
        .and_then(|t| t.get("union_cover"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("audioId".into(), json!(val_u64(raw, "audio_id")));
    if let Some(aaid) = val_str_or_num(raw, "album_audio_id") {
        out.insert("albumAudioId".into(), json!(aaid));
    }
    out.insert("hash".into(), json!(val_str(raw, "hash")));
    out.insert("name".into(), json!(name));
    out.insert("artist".into(), json!(artist_name));
    out.insert("artists".into(), json!(artists));
    out.insert(
        "album".into(),
        json!(decode_name(&val_str(raw, "album_name"))),
    );
    out.insert(
        "albumId".into(),
        json!(val_str_or_num(raw, "album_id").unwrap_or_default()),
    );
    out.insert("cover".into(), json!(fill_cover(cover_tpl, 300)));
    out.insert("coverOriginal".into(), json!(fill_cover(cover_tpl, 480)));
    let interval = val_u64(raw, "duration");
    out.insert("interval".into(), json!(interval));
    out.insert("duration".into(), json!(interval * 1000));
    out.insert("qualities".into(), json!(qualities));
    out.insert("hashes".into(), Value::Object(hashes));
    out.insert("sizes".into(), Value::Object(sizes));
    out.insert(
        "pay".into(),
        json!({
            "payplay": val_i64(raw, "pay_type"),
            "privilege": val_i64(raw, "privilege"),
            "feetype": val_i64(raw, "feetype"),
            "pkg_price": val_i64(raw, "pkg_price"),
            "price": val_i64(raw, "price"),
        }),
    );
    Value::Object(out)
}

fn normalize_from_legacy(raw: &Value) -> Value {
    let (qualities, hashes, sizes) = {
        // legacy 字段名不同：FileHash/HQFileHash/SQFileHash/ResFileHash
        let mut qualities = Vec::new();
        let mut hashes = Map::new();
        let mut sizes = Map::new();
        let mut push = |key: &str, hash: &str, size: &str| {
            let h = val_str_truthy(raw, hash);
            let s = val_num_truthy(raw, size);
            if let (Some(h), Some(s)) = (h, s) {
                qualities.push(key.to_string());
                hashes.insert(key.to_string(), json!(h));
                sizes.insert(key.to_string(), num_json(s));
            }
        };
        push("128k", "FileHash", "FileSize");
        push("320k", "HQFileHash", "HQFileSize");
        push("flac", "SQFileHash", "SQFileSize");
        push("flac24bit", "ResFileHash", "ResFileSize");
        (qualities, hashes, sizes)
    };

    let singers = raw.get("Singers").and_then(Value::as_array).cloned().unwrap_or_default();
    let artist_name = singers
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .map(decode_name)
        .collect::<Vec<_>>()
        .join(" / ");
    let artists: Vec<Value> = if !singers.is_empty() {
        singers
            .iter()
            .map(|s| {
                json!({
                    "id": val_str_or_num(s, "id").or_else(|| s.get("name").and_then(Value::as_str).map(String::from)).unwrap_or_default(),
                    "name": decode_name(&val_str(s, "name")),
                })
            })
            .collect()
    } else if !artist_name.is_empty() {
        vec![json!({ "id": artist_name, "name": artist_name })]
    } else {
        vec![]
    };

    let id = {
        let audio_id = raw.get("Audioid").and_then(Value::as_u64).unwrap_or_default();
        if audio_id != 0 {
            audio_id.to_string()
        } else {
            val_str_truthy(raw, "FileHash").unwrap_or_default()
        }
    };

    let interval = val_u64(raw, "Duration");
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("audioId".into(), json!(val_u64(raw, "Audioid")));
    if let Some(mix) = val_str_or_num(raw, "MixSongID") {
        out.insert("albumAudioId".into(), json!(mix));
    } else if let Some(aa) = val_str_or_num(raw, "AlbumAudioId") {
        out.insert("albumAudioId".into(), json!(aa));
    }
    out.insert("hash".into(), json!(val_str(raw, "FileHash")));
    out.insert("name".into(), json!(decode_name(&val_str(raw, "SongName"))));
    out.insert("artist".into(), json!(artist_name));
    out.insert("artists".into(), json!(artists));
    out.insert("album".into(), json!(decode_name(&val_str(raw, "AlbumName"))));
    out.insert("albumId".into(), json!(val_str_or_num(raw, "AlbumID").unwrap_or_default()));
    out.insert("interval".into(), json!(interval));
    out.insert("duration".into(), json!(interval * 1000));
    out.insert("qualities".into(), json!(qualities));
    out.insert("hashes".into(), Value::Object(hashes));
    out.insert("sizes".into(), Value::Object(sizes));
    out.insert(
        "pay".into(),
        json!({
            "payplay": val_i64(raw, "PayType"),
            "privilege": val_i64(raw, "Privilege"),
            "pkg_price": val_i64(raw, "PkgPrice"),
            "price": val_i64(raw, "Price"),
        }),
    );
    Value::Object(out)
}

/// group 数组展开 + 按 `audioId_hash` 去重（对齐桌面端 push/dedup 循环）。
fn flatten_with_dedup(
    raw_list: &[Value],
    group_key: &str,
    normalize: impl Fn(&Value) -> Value,
    key_of: impl Fn(&Value) -> String,
) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut push = |item: &Value| {
        let key = key_of(item);
        if seen.insert(key) {
            out.push(normalize(item));
        }
    };
    for item in raw_list {
        push(item);
        if let Some(group) = item.get(group_key).and_then(Value::as_array) {
            for sub in group {
                push(sub);
            }
        }
    }
    out
}

impl KugouClient {
    pub async fn search(&self, p: &SearchParams) -> Result<Value, QqkgError> {
        if p.keyword.is_empty() {
            return Ok(json!({ "code": 400, "total": 0, "message": "keywords required" }));
        }
        match p.ty {
            SearchType::Song => self.search_songs(p).await,
            SearchType::Album => self.search_albums(p).await,
            SearchType::Artist => self.search_artists(p).await,
            SearchType::Playlist => self.search_playlists(p).await,
        }
    }

    async fn search_songs(&self, p: &SearchParams) -> Result<Value, QqkgError> {
        // 主路径：mobilecdn v3（含封面与音质分级）
        let mobile_url = format!(
            "{KG_MOBILECDN_URL}?keyword={}&page={}&pagesize={}&format=json&showtype=1",
            urlencoding::encode(&p.keyword),
            p.page,
            p.limit
        );
        let mobile_result: Result<Value, QqkgError> = self.kg_request(&mobile_url).await;
        if let Ok(body) = mobile_result {
            let raw_list = body
                .pointer("/data/info")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let total = body
                .pointer("/data/total")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let songs = flatten_with_dedup(&raw_list, "group", normalize_from_mobile, |item| {
                format!(
                    "{}_{}",
                    item.get("audio_id").and_then(Value::as_u64).unwrap_or_default(),
                    val_str(item, "hash")
                )
            });
            if !songs.is_empty() {
                return Ok(json!({
                    "code": 200,
                    "total": if total > 0 { total } else { songs.len() as u64 },
                    "songs": songs
                }));
            }
        }

        // 兜底：legacy songsearch
        let legacy_url = format!(
            "{KG_LEGACY_SEARCH_URL}?keyword={}&page={}&pagesize={}&userid=0&clientver=&platform=WebFilter&filter=2&iscorrection=1&privilege_filter=0&area_code=1",
            urlencoding::encode(&p.keyword),
            p.page,
            p.limit
        );
        let body = self.kg_request(&legacy_url).await?;
        let raw_list = body
            .pointer("/data/lists")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = body
            .pointer("/data/total")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let songs = flatten_with_dedup(&raw_list, "Grp", normalize_from_legacy, |item| {
            format!(
                "{}_{}",
                item.get("Audioid").and_then(Value::as_u64).unwrap_or_default(),
                val_str(item, "FileHash")
            )
        });
        Ok(json!({
            "code": 200,
            "total": if total > 0 { total } else { songs.len() as u64 },
            "songs": songs
        }))
    }

    async fn search_gateway(
        &self,
        p: &SearchParams,
        path: &str,
        extra: &[(&str, &str)],
    ) -> Result<Value, QqkgError> {
        let mut params: Vec<(String, String)> = vec![
            ("keyword".into(), p.keyword.clone()),
            ("page".into(), p.page.to_string()),
            ("pagesize".into(), p.limit.to_string()),
            ("platform".into(), "AndroidFilter".into()),
            ("iscorrection".into(), "1".into()),
        ];
        for (k, v) in extra {
            params.push((k.to_string(), v.to_string()));
        }
        let headers = [(KG_X_ROUTER, COMPLEXSEARCH.to_string())];
        self.kg_gateway_request(path, &params, &headers).await
    }

    async fn search_albums(&self, p: &SearchParams) -> Result<Value, QqkgError> {
        let res = self
            .search_gateway(
                p,
                "/v1/search/album",
                &[("albumhide", "0"), ("nocollect", "0")],
            )
            .await?;
        let raw_list = gateway_list(&res);
        let total = res.pointer("/data/total").and_then(Value::as_u64).unwrap_or_default();
        let albums: Vec<Value> = raw_list
            .iter()
            .map(|raw| {
                let singer_str = raw
                    .get("singer")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| {
                        raw.get("singers")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|s| s.get("name").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join(" / ")
                            })
                            .unwrap_or_default()
                    });
                let mut out = Map::new();
                out.insert(
                    "id".into(),
                    json!(val_str_or_num(raw, "albumid").unwrap_or_default()),
                );
                out.insert("name".into(), json!(decode_name(&val_str(raw, "albumname"))));
                out.insert("cover".into(), json!(fill_cover(&val_str(raw, "img"), 300)));
                out.insert("artist".into(), json!(decode_name(&singer_str)));
                if let Some(artist_id) = val_str_or_num(raw, "singerid")
                    .or_else(|| raw.get("singerids").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_f64).map(|n| n.to_string()))
                {
                    out.insert("artistId".into(), json!(artist_id));
                }
                out.insert("trackCount".into(), json!(val_u64(raw, "songcount")));
                if let Some(pt) = val_str_truthy(raw, "publish_time") {
                    out.insert("publishTime".into(), json!(pt));
                }
                if let Some(intro) = val_str_truthy(raw, "intro") {
                    out.insert("intro".into(), json!(decode_name(&intro)));
                }
                Value::Object(out)
            })
            .collect();
        Ok(json!({
            "code": 200,
            "total": if total > 0 { total } else { albums.len() as u64 },
            "albums": albums
        }))
    }

    async fn search_artists(&self, p: &SearchParams) -> Result<Value, QqkgError> {
        let res = self.search_gateway(p, "/v1/search/author", &[]).await?;
        let raw_list = gateway_list(&res);
        let total = res.pointer("/data/total").and_then(Value::as_u64).unwrap_or_default();
        let artists: Vec<Value> = raw_list
            .iter()
            .map(|raw| {
                let cover = raw
                    .get("Avatar")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| raw.get("FirstFrameImage").and_then(Value::as_str))
                    .unwrap_or_default();
                json!({
                    "id": val_str_or_num(raw, "AuthorId").unwrap_or_default(),
                    "name": decode_name(&val_str(raw, "AuthorName")),
                    "cover": fill_cover(cover, 300),
                    "albumCount": val_u64(raw, "AlbumCount"),
                    "songCount": val_u64(raw, "AudioCount"),
                    "fansCount": val_u64(raw, "FansNum"),
                })
            })
            .collect();
        Ok(json!({
            "code": 200,
            "total": if total > 0 { total } else { artists.len() as u64 },
            "artists": artists
        }))
    }

    async fn search_playlists(&self, p: &SearchParams) -> Result<Value, QqkgError> {
        let res = self.search_gateway(p, "/v1/search/special", &[]).await?;
        let raw_list = gateway_list(&res);
        let total = res.pointer("/data/total").and_then(Value::as_u64).unwrap_or_default();
        let playlists: Vec<Value> = raw_list
            .iter()
            .map(|raw| {
                let play_count = raw
                    .get("play_count")
                    .and_then(Value::as_f64)
                    .filter(|n| *n != 0.0)
                    .or_else(|| raw.get("total_play_count").and_then(Value::as_f64))
                    .unwrap_or_default();
                let id = val_str_or_num(raw, "specialid")
                    .filter(|s| !s.is_empty())
                    .or_else(|| val_str_or_num(raw, "gid"))
                    .unwrap_or_default();
                json!({
                    "id": id,
                    "name": decode_name(&val_str(raw, "specialname")),
                    "cover": fill_cover(&val_str(raw, "img"), 300),
                    "creator": decode_name(&val_str(raw, "nickname")),
                    "trackCount": val_u64(raw, "song_count"),
                    "playCount": play_count as u64,
                })
            })
            .collect();
        Ok(json!({
            "code": 200,
            "total": if total > 0 { total } else { playlists.len() as u64 },
            "playlists": playlists
        }))
    }
}

/// 网关响应的列表字段：`data.lists` 或 `data.info`（对齐桌面端 `?? res.data?.info`）。
fn gateway_list(res: &Value) -> Vec<Value> {
    res.pointer("/data/lists")
        .and_then(Value::as_array)
        .or_else(|| res.pointer("/data/info").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mobile_artist_splits_separators() {
        assert_eq!(format_mobile_artist("周杰伦、费玉清"), "周杰伦 / 费玉清");
        assert_eq!(format_mobile_artist("A,B;C/D"), "A / B / C / D");
        assert_eq!(format_mobile_artist(" &nbsp; "), "");
        assert_eq!(format_mobile_artist("群星"), "群星");
    }

    #[test]
    fn normalize_from_mobile_full_mapping() {
        let raw = json!({
            "hash": "abc123",
            "audio_id": 9105762,
            "album_audio_id": 33,
            "songname": "晴天&nbsp;",
            "singername": "周杰伦、稻香",
            "album_id": 1483926,
            "album_name": "叶惠美",
            "duration": 269,
            "filesize": 4312323,
            "320hash": "hq123",
            "320filesize": 4304861,
            "sqhash": "sq123",
            "sqfilesize": 28332475,
            "pay_type": 1,
            "privilege": 10,
            "trans_param": { "union_cover": "http://imge.kugou.com/stdmusic/{size}/cover.jpg" }
        });
        let out = normalize_from_mobile(&raw);
        assert_eq!(out["id"], json!("9105762"));
        assert_eq!(out["audioId"], json!(9105762));
        assert_eq!(out["albumAudioId"], json!("33"));
        assert_eq!(out["hash"], json!("abc123"));
        assert_eq!(out["name"], json!("晴天 ")); // &nbsp; → 空格
        assert_eq!(out["artist"], json!("周杰伦 / 稻香"));
        assert_eq!(out["artists"].as_array().unwrap().len(), 2);
        assert_eq!(out["album"], json!("叶惠美"));
        assert_eq!(out["albumId"], json!("1483926"));
        assert_eq!(out["cover"], json!("https://imge.kugou.com/stdmusic/300/cover.jpg"));
        assert_eq!(out["coverOriginal"], json!("https://imge.kugou.com/stdmusic/480/cover.jpg"));
        assert_eq!(out["interval"], json!(269));
        assert_eq!(out["duration"], json!(269000));
        assert_eq!(out["qualities"], json!(["128k", "320k", "flac"]));
        assert_eq!(out["hashes"]["flac"], json!("sq123"));
        assert_eq!(out["sizes"]["320k"], json!(4304861));
        assert_eq!(out["pay"]["payplay"], json!(1));
    }

    #[test]
    fn normalize_from_mobile_falsy_fallbacks() {
        // audio_id 为 0 → id 回落到 hash
        let raw = json!({ "hash": "H1", "audio_id": 0, "songname": "", "filename": "fname.mp3" });
        let out = normalize_from_mobile(&raw);
        assert_eq!(out["id"], json!("H1"));
        assert_eq!(out["name"], json!("fname.mp3"));
        assert_eq!(out["qualities"], json!([]));
        assert!(out.get("albumAudioId").is_none());
        assert_eq!(out["cover"], json!(""));
    }

    #[test]
    fn normalize_from_legacy_mapping() {
        let raw = json!({
            "Audioid": 111,
            "MixSongID": 222,
            "SongName": "Legacy&nbsp;Song",
            "Singers": [{ "id": 5, "name": "歌手A" }, { "id": 6, "name": "歌手B" }],
            "AlbumName": "Album&amp;X",
            "Duration": 100,
            "FileHash": "FH",
            "FileSize": 1000,
            "HQFileHash": "HQ",
            "HQFileSize": 2000,
        });
        let out = normalize_from_legacy(&raw);
        assert_eq!(out["id"], json!("111"));
        assert_eq!(out["albumAudioId"], json!("222"));
        assert_eq!(out["name"], json!("Legacy Song"));
        assert_eq!(out["artist"], json!("歌手A / 歌手B"));
        assert_eq!(out["artists"][0]["id"], json!("5"));
        assert_eq!(out["album"], json!("Album&X"));
        assert_eq!(out["qualities"], json!(["128k", "320k"]));
        assert!(out.get("cover").is_none());
        // legacy pay 无 feetype 键（对齐桌面端 normalizeFromLegacy）
        assert!(out["pay"].get("feetype").is_none());
    }

    #[test]
    fn flatten_dedup_expands_group() {
        let raw_list = vec![
            json!({ "audio_id": 1, "hash": "a", "group": [
                { "audio_id": 2, "hash": "b" },
                { "audio_id": 1, "hash": "a" }
            ]}),
            json!({ "audio_id": 2, "hash": "b" }),
        ];
        let out = flatten_with_dedup(&raw_list, "group", |v| v.clone(), |item| {
            format!(
                "{}_{}",
                item.get("audio_id").and_then(Value::as_u64).unwrap_or_default(),
                val_str(item, "hash")
            )
        });
        assert_eq!(out.len(), 2); // group 内重复与外层重复都被去掉
    }
}
