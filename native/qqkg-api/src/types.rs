use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 酷狗客户端常量（对齐桌面端 kugou/core/config.ts）。
pub const KG_APPID: u32 = 1005;
pub const KG_CLIENTVER: u32 = 20489;

/// 统一音乐平台用户信息（对齐前端 PlatformProfile）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformProfile {
    pub user_id: String,
    pub nickname: String,
    pub avatar_url: String,
    pub is_vip: bool,
    #[serde(default)]
    pub vip_level: i64,
}

/// 用户详情统一响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDetailResponse {
    pub code: i32,
    pub logged_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<PlatformProfile>,
}

impl UserDetailResponse {
    pub fn logged_in(profile: PlatformProfile) -> Self {
        Self {
            code: 200,
            logged_in: true,
            message: None,
            profile: Some(profile),
        }
    }

    pub fn not_logged_in(message: Option<&str>) -> Self {
        Self {
            code: 200,
            logged_in: false,
            message: message.map(ToString::to_string),
            profile: None,
        }
    }
}

/// 酷狗扫码 Key 响应（对齐前端 KugouQrKeyResponse）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KugouQrKeyResponse {
    pub code: i32,
    pub key: String,
    pub content: String,
}

/// 酷狗扫码状态轮询响应（对齐前端 KugouQrCheckResponse 及凭据下发）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KugouQrCheckResponse {
    pub code: i32,
    pub status: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vip_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vip_type: Option<String>,
}


/// 搜索类型（前端 type 参数的内部表示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchType {
    Song,
    Album,
    Artist,
    Playlist,
}

impl SearchType {
    /// 从 dispatch 层透传的 type 参数解析（兼容数字 0/2/3/8/9 与字符串形态）。
    pub fn from_value(v: Option<&Value>) -> Self {
        let n = v
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok())))
            .unwrap_or(0);
        match n {
            8 => Self::Album,
            9 => Self::Artist,
            2 | 3 => Self::Playlist,
            _ => Self::Song,
        }
    }

    /// QQ 上游 search_type 映射：单曲 0 / 歌手 1 / 专辑 2 / 歌单 3（对齐桌面端）。
    pub fn qq_search_type(self) -> i64 {
        match self {
            Self::Song => 0,
            Self::Artist => 1,
            Self::Album => 2,
            Self::Playlist => 3,
        }
    }
}

/// 搜索请求参数。
#[derive(Debug, Clone)]
pub struct SearchParams {
    pub keyword: String,
    pub page: u32,
    pub limit: u32,
    pub ty: SearchType,
}

impl SearchParams {
    /// 从 dispatch 的 params HashMap 解析。参数名兼容 headless 现有约定
    /// （keyword|keywords、page、pageSize|limit），对齐 online_apis.rs 的既有解析逻辑。
    pub fn from_map(params: &HashMap<String, Value>) -> Self {
        let keyword = params
            .get("keyword")
            .or_else(|| params.get("keywords"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let page = params.get("page").and_then(Value::as_u64).unwrap_or(1).max(1) as u32;
        let limit = params
            .get("pageSize")
            .or_else(|| params.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100) as u32;
        let ty = SearchType::from_value(params.get("type"));
        Self {
            keyword,
            page,
            limit,
            ty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn search_type_from_numeric_and_string() {
        assert_eq!(SearchType::from_value(Some(&Value::from(0))), SearchType::Song);
        assert_eq!(SearchType::from_value(Some(&Value::from(8))), SearchType::Album);
        assert_eq!(SearchType::from_value(Some(&Value::from(9))), SearchType::Artist);
        assert_eq!(SearchType::from_value(Some(&Value::from(2))), SearchType::Playlist);
        assert_eq!(SearchType::from_value(Some(&Value::from(3))), SearchType::Playlist);
        assert_eq!(SearchType::from_value(Some(&Value::from("8"))), SearchType::Album);
        assert_eq!(SearchType::from_value(None), SearchType::Song);
        assert_eq!(SearchType::from_value(Some(&Value::from(999))), SearchType::Song);
    }

    #[test]
    fn qq_search_type_mapping() {
        assert_eq!(SearchType::Song.qq_search_type(), 0);
        assert_eq!(SearchType::Artist.qq_search_type(), 1);
        assert_eq!(SearchType::Album.qq_search_type(), 2);
        assert_eq!(SearchType::Playlist.qq_search_type(), 3);
    }

    #[test]
    fn search_params_defaults_and_aliases() {
        let p = SearchParams::from_map(&map(&[]));
        assert_eq!((p.keyword.as_str(), p.page, p.limit), ("", 1, 20));
        assert_eq!(p.ty, SearchType::Song);

        let p = SearchParams::from_map(&map(&[
            ("keywords", Value::from("周杰伦")),
            ("page", Value::from(3)),
            ("limit", Value::from(30)),
            ("type", Value::from(8)),
        ]));
        assert_eq!(p.keyword, "周杰伦");
        assert_eq!((p.page, p.limit), (3, 30));
        assert_eq!(p.ty, SearchType::Album);

        // keyword 命中时不应回落到 keywords
        let p = SearchParams::from_map(&map(&[
            ("keyword", Value::from("A")),
            ("keywords", Value::from("B")),
        ]));
        assert_eq!(p.keyword, "A");

        // limit 越界钳制
        let p = SearchParams::from_map(&map(&[("limit", Value::from(500))]));
        assert_eq!(p.limit, 100);
    }
}
