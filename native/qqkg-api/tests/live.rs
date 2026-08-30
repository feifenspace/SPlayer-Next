//! 临时联网联调测试（验证后删除）：真实调用 QQ/酷狗上游搜索接口。

use std::collections::HashMap;

use qqkg_api::{KugouClient, QqmusicClient, SearchParams};

fn sp(keyword: &str, ty: u64) -> SearchParams {
    let mut m = HashMap::new();
    m.insert("keywords".to_string(), serde_json::json!(keyword));
    m.insert("page".to_string(), serde_json::json!(1));
    m.insert("limit".to_string(), serde_json::json!(5));
    m.insert("type".to_string(), serde_json::json!(ty));
    SearchParams::from_map(&m)
}

fn count(v: &serde_json::Value, key: &str) -> usize {
    v.get(key).and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0)
}

#[tokio::test]
async fn qq_live_search_all_types() {
    let c = QqmusicClient::new(HashMap::new());

    let r = c.search(&sp("周杰伦", 0)).await.unwrap();
    assert_eq!(r["code"], 200);
    assert!(count(&r, "songs") > 0, "qq songs empty: {r}");
    let s = &r["songs"][0];
    println!("qq song: {} - {} album={} dur={} cover={}", s["mid"], s["name"], s["album"], s["duration"], s["cover"]);
    assert!(s["name"].as_str().unwrap().len() > 0);
    assert!(s["duration"].as_u64().unwrap() > 0);
    assert!(s["cover"].as_str().unwrap().starts_with("https://"));

    let r = c.search(&sp("周杰伦", 8)).await.unwrap();
    assert!(count(&r, "albums") > 0, "qq albums empty: {r}");
    println!("qq album: {} cover={}", r["albums"][0]["name"], r["albums"][0]["cover"]);

    let r = c.search(&sp("周杰伦", 9)).await.unwrap();
    assert!(count(&r, "artists") > 0, "qq artists empty: {r}");
    println!("qq artist: {} cover={}", r["artists"][0]["name"], r["artists"][0]["cover"]);

    let r = c.search(&sp("周杰伦", 2)).await.unwrap();
    assert!(count(&r, "playlists") > 0, "qq playlists empty: {r}");
    println!("qq playlist: {} creator={}", r["playlists"][0]["name"], r["playlists"][0]["creator"]);
}

#[tokio::test]
async fn kugou_live_search_all_types() {
    let c = KugouClient::new(HashMap::new());

    let r = c.search(&sp("周杰伦", 0)).await.unwrap();
    assert_eq!(r["code"], 200);
    assert!(count(&r, "songs") > 0, "kg songs empty: {r}");
    let s = &r["songs"][0];
    println!("kg song: hash={} name={} artist={} qualities={:?} cover={}",
        s["hash"], s["name"], s["artist"], s["qualities"], s["cover"]);
    assert!(s["hash"].as_str().unwrap().len() > 0);
    assert!(s["duration"].as_u64().unwrap() > 0);
    assert!(s["qualities"].as_array().unwrap().len() > 0);

    let r = c.search(&sp("周杰伦", 8)).await.unwrap();
    assert!(count(&r, "albums") > 0, "kg albums empty: {r}");
    println!("kg album: {} artist={}", r["albums"][0]["name"], r["albums"][0]["artist"]);

    let r = c.search(&sp("周杰伦", 9)).await.unwrap();
    assert!(count(&r, "artists") > 0, "kg artists empty: {r}");
    println!("kg artist: {} fans={}", r["artists"][0]["name"], r["artists"][0]["fansCount"]);

    let r = c.search(&sp("周杰伦", 2)).await.unwrap();
    assert!(count(&r, "playlists") > 0, "kg playlists empty: {r}");
    println!("kg playlist: {} creator={}", r["playlists"][0]["name"], r["playlists"][0]["creator"]);
}

#[tokio::test]
async fn kugou_live_login_qr_key_and_check() {
    let c = KugouClient::new(HashMap::new());
    let qr_key_res = c.login_qr_key().await.unwrap();
    assert_eq!(qr_key_res.code, 200);
    assert!(!qr_key_res.key.is_empty());
    assert!(qr_key_res.content.starts_with("https://h5.kugou.com/"));
    println!("kugou qr key: {} url: {}", qr_key_res.key, qr_key_res.content);

    let check_res = c.login_qr_check(&qr_key_res.key).await.unwrap();
    assert_eq!(check_res.code, 200);
    println!("kugou check status: {}", check_res.status);
}

#[tokio::test]
async fn qq_live_user_detail_fallback() {
    let mut cookies = HashMap::new();
    cookies.insert("uin".to_string(), "10001".to_string());
    cookies.insert("qm_keyst".to_string(), "dummy_token".to_string());
    let c = QqmusicClient::new(cookies);
    let res = c.user_detail().await.unwrap();
    assert_eq!(res.code, 200);
    assert_eq!(res.logged_in, true);
    let profile = res.profile.unwrap();
    assert_eq!(profile.user_id, "10001");
    assert!(profile.avatar_url.contains("qlogo.cn"));
    println!("qq user profile: {:?}", profile);
}

