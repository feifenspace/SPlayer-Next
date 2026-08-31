//! 酷狗歌手详情、热门歌曲与专辑列表模块（对齐桌面端 electron/main/apis/kugou/modules/artist.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::normalize::{decode_name, fill_cover_opt};

impl KugouClient {

    /// 自动将歌手名解析为数字 author_id（若传入的已为数字则直接返回）。
    async fn resolve_author_id(&self, id_or_name: &str) -> String {
        if id_or_name.chars().all(|c| c.is_ascii_digit()) && !id_or_name.is_empty() {
            return id_or_name.to_string();
        }

        let search_params = vec![
            ("keyword".into(), id_or_name.to_string()),
            ("page".into(), "1".into()),
            ("pagesize".into(), "1".into()),
            ("platform".into(), "AndroidFilter".into()),
            ("iscorrection".into(), "1".into()),
        ];

        if let Ok(resp) = self
            .kg_gateway_request(
                "/v1/search/author",
                &search_params,
                &[("x-router", "complexsearch.kugou.com".into())],
            )
            .await
        {
            if let Some(first) = resp
                .get("data")
                .and_then(|d| d.get("lists"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
            {
                if let Some(aid) = first.get("AuthorId").and_then(|v| {
                    v.as_u64()
                        .map(|n| n.to_string())
                        .or_else(|| v.as_str().map(ToString::to_string))
                }) {
                    return aid;
                }
            }
        }

        id_or_name.to_string()
    }

    /// 获取酷狗歌手详情、热门单曲与专辑列表。
    pub async fn artist(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let raw_id = params
            .get("id")
            .or_else(|| params.get("singerid"))
            .or_else(|| params.get("author_id"))
            .or_else(|| params.get("singer_id"))
            .or_else(|| params.get("artistId"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if raw_id.is_empty() {
            return Err(QqkgError::InvalidParam("singer id required".into()));
        }

        let id = self.resolve_author_id(&raw_id).await;
        let page = params.get("page").and_then(Value::as_u64).unwrap_or(1);
        let limit = params
            .get("limit")
            .or_else(|| params.get("pagesize"))
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 100);

        let info_url = format!(
            "http://mobilecdn.kugou.com/api/v3/singer/info?singerid={}&format=json",
            urlencoding::encode(&id)
        );
        let song_url = format!(
            "http://mobilecdn.kugou.com/api/v3/singer/song?singerid={}&page={}&pagesize={}&format=json",
            urlencoding::encode(&id),
            page,
            limit
        );
        let album_url = format!(
            "http://mobilecdn.kugou.com/api/v3/singer/album?singerid={}&page=1&pagesize=50&format=json",
            urlencoding::encode(&id)
        );

        let info_future = self.kg_request(&info_url);
        let song_future = self.kg_request(&song_url);
        let album_future = self.kg_request(&album_url);

        let (info_res, song_res, album_res) = tokio::join!(info_future, song_future, album_future);
        let info_body = info_res.unwrap_or(json!({}));
        let song_body = song_res.unwrap_or(json!({}));
        let album_body = album_res.unwrap_or(json!({}));

        let info = info_body.get("data").cloned().unwrap_or(Value::Null);
        let empty_vec = Vec::new();
        let raw_songs = song_body
            .get("data")
            .and_then(|d| d.get("info"))
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);

        let raw_albums = album_body
            .get("data")
            .and_then(|d| d.get("info"))
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);

        let img_url = info.get("imgurl").and_then(Value::as_str);
        let cover = fill_cover_opt(img_url, 300);
        let cover_orig = fill_cover_opt(img_url, 480);

        let mut songs: Vec<Value> = Vec::new();
        for raw in raw_songs {
            let mut song_obj = crate::kugou::search::normalize_from_mobile(raw);
            if song_obj.get("cover").and_then(Value::as_str).unwrap_or("").is_empty() {
                if let Some(obj) = song_obj.as_object_mut() {
                    obj.insert("cover".into(), json!(cover));
                    obj.insert("coverOriginal".into(), json!(cover_orig));
                }
            }
            songs.push(song_obj);
        }


        let mut albums: Vec<Value> = Vec::new();
        for raw in raw_albums {
            let alb_id = raw.get("albumid").and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            }).unwrap_or_default();

            let alb_img = raw.get("imgurl").and_then(Value::as_str);

            albums.push(json!({
                "id": alb_id,
                "name": decode_name(raw.get("albumname").and_then(Value::as_str).unwrap_or("")),
                "artist": decode_name(raw.get("singername").and_then(Value::as_str).unwrap_or("")),
                "cover": fill_cover_opt(alb_img, 300),
                "trackCount": raw.get("songcount").and_then(Value::as_u64).unwrap_or(0),
                "publishTime": raw.get("publishtime").and_then(Value::as_str).unwrap_or(""),
            }));
        }

        let artist_name = decode_name(info.get("singername").and_then(Value::as_str).unwrap_or(&raw_id));

        Ok(json!({
            "code": 200,
            "artist": {
                "id": id,
                "name": artist_name,
                "intro": info.get("intro").and_then(Value::as_str).unwrap_or(""),
                "cover": cover,
                "avatar": cover_orig,
                "songCount": info.get("songcount").and_then(Value::as_u64).unwrap_or(songs.len() as u64),
                "albumCount": info.get("albumcount").and_then(Value::as_u64).unwrap_or(albums.len() as u64),
            },
            "songs": songs,
            "albums": albums,
        }))

    }
}
