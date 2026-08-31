use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::QobuzClient;

impl QobuzClient {
    /// 获取用户收藏 (tracks, albums, artists, playlists)
    pub async fn user_get_favorites(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let fav_type = params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("tracks,albums,artists,playlists");

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(100);

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);

        let mut req_params = HashMap::new();
        req_params.insert("type".to_string(), fav_type.to_string());
        req_params.insert("limit".to_string(), limit.to_string());
        req_params.insert("offset".to_string(), offset.to_string());

        self.request("favorite/getUserFavorites", req_params, false, true, None).await
    }

    /// 添加收藏
    pub async fn favorite_create(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let mut req_params = HashMap::new();
        if let Some(track_ids) = params.get("track_ids").and_then(Value::as_str) {
            req_params.insert("track_ids".to_string(), track_ids.to_string());
        }
        if let Some(album_ids) = params.get("album_ids").and_then(Value::as_str) {
            req_params.insert("album_ids".to_string(), album_ids.to_string());
        }
        if let Some(artist_ids) = params.get("artist_ids").and_then(Value::as_str) {
            req_params.insert("artist_ids".to_string(), artist_ids.to_string());
        }
        if let Some(playlist_ids) = params.get("playlist_ids").and_then(Value::as_str) {
            req_params.insert("playlist_ids".to_string(), playlist_ids.to_string());
        }

        self.request("favorite/create", req_params, false, true, None).await
    }

    /// 取消收藏
    pub async fn favorite_delete(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let mut req_params = HashMap::new();
        if let Some(track_ids) = params.get("track_ids").and_then(Value::as_str) {
            req_params.insert("track_ids".to_string(), track_ids.to_string());
        }
        if let Some(album_ids) = params.get("album_ids").and_then(Value::as_str) {
            req_params.insert("album_ids".to_string(), album_ids.to_string());
        }
        if let Some(artist_ids) = params.get("artist_ids").and_then(Value::as_str) {
            req_params.insert("artist_ids".to_string(), artist_ids.to_string());
        }
        if let Some(playlist_ids) = params.get("playlist_ids").and_then(Value::as_str) {
            req_params.insert("playlist_ids".to_string(), playlist_ids.to_string());
        }

        self.request("favorite/delete", req_params, false, true, None).await
    }
}
