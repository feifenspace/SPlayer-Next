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

#[tokio::test]
async fn kugou_live_song_url() {
    let c = KugouClient::new(HashMap::new());
    // 搜索周杰伦《晴天》
    let search_res = c.search(&sp("晴天", 0)).await.unwrap();
    assert_eq!(search_res["code"], 200);
    let songs = search_res["songs"].as_array().unwrap();
    assert!(!songs.is_empty());
    let song = &songs[0];
    let hash = song["hash"].as_str().unwrap();

    let mut p = HashMap::new();
    p.insert("hash".to_string(), serde_json::json!(hash));
    p.insert("audioId".to_string(), song["audioId"].clone());
    p.insert("albumId".to_string(), song["albumId"].clone());
    p.insert("level".to_string(), serde_json::json!("hq"));
    let res = c.song_url(&p).await.unwrap();
    println!("kugou song_url res for {}: {:?}", hash, res);
    assert_eq!(res["code"], 200);
    let url = res["data"]["url"].as_str().unwrap();
    assert!(url.starts_with("http"));
    println!("kugou playable URL: {}", url);
}


#[tokio::test]
async fn qq_live_song_url() {
    let c = QqmusicClient::new(HashMap::new());
    let mut p = HashMap::new();
    p.insert("mid".to_string(), serde_json::json!("0039MnYb0qxYhV"));
    p.insert("mediaMid".to_string(), serde_json::json!("0039MnYb0qxYhV"));
    p.insert("level".to_string(), serde_json::json!("hq"));
    let res = c.song_url(&p).await.unwrap();
    println!("qq song_url res: {:?}", res);
    assert!(res["code"] == 200 || res["code"] == 403);
}

#[tokio::test]
async fn qq_live_m4_browsing_endpoints() {
    let c = QqmusicClient::new(HashMap::new());

    // 1. 热搜
    let hot = c.hot_search().await.unwrap();
    assert_eq!(hot["code"], 200);
    assert!(hot["list"].as_array().unwrap().len() > 0);
    println!("qq hot search count: {}", hot["list"].as_array().unwrap().len());

    // 2. 排行榜
    let mut top_p = HashMap::new();
    top_p.insert("topid".to_string(), serde_json::json!(4));
    let lb = c.leaderboard(&top_p).await.unwrap();
    println!("qq leaderboard res: {:?}", lb);
    assert_eq!(lb["code"], 200);


    // 3. 专辑
    let mut alb_p = HashMap::new();
    alb_p.insert("mid".to_string(), serde_json::json!("000MkMni19ClKG"));
    let alb = c.album(&alb_p).await.unwrap();
    assert_eq!(alb["code"], 200);
    assert!(alb["songs"].as_array().unwrap().len() > 0);
    println!("qq album songs: {}", alb["songs"].as_array().unwrap().len());


    // 4. 歌手
    let mut art_p = HashMap::new();
    art_p.insert("mid".to_string(), serde_json::json!("0025NhlN2yWrP4"));
    let art = c.artist(&art_p).await.unwrap();
    assert_eq!(art["code"], 200);
    assert!(art["songs"].as_array().unwrap().len() > 0);
    println!("qq artist songs: {} albums: {}", art["songs"].as_array().unwrap().len(), art["albums"].as_array().unwrap().len());
}

#[tokio::test]
async fn kugou_live_m4_browsing_endpoints() {
    let c = KugouClient::new(HashMap::new());

    // 1. 歌单
    let mut pl_p = HashMap::new();
    pl_p.insert("id".to_string(), serde_json::json!("878985"));
    let pl = c.playlist(&pl_p).await.unwrap();
    assert_eq!(pl["code"], 200);
    println!("kugou playlist: {} songs: {}", pl["name"], pl["songs"].as_array().unwrap().len());

    // 2. 专辑
    let mut alb_p = HashMap::new();
    alb_p.insert("id".to_string(), serde_json::json!("960537"));
    let alb = c.album(&alb_p).await.unwrap();
    assert_eq!(alb["code"], 200);
    println!("kugou album: {} songs: {}", alb["name"], alb["songs"].as_array().unwrap().len());

    // 3. 歌手 (按数字 ID)
    let mut art_p = HashMap::new();
    art_p.insert("id".to_string(), serde_json::json!("3060"));
    let art = c.artist(&art_p).await.unwrap();
    assert_eq!(art["code"], 200);
    println!("kugou artist (by id): {} songs: {}", art["artist"]["name"], art["songs"].as_array().unwrap().len());

    // 4. 歌手 (按中文名)
    let mut art_name_p = HashMap::new();
    art_name_p.insert("id".to_string(), serde_json::json!("周杰伦"));
    let art2 = c.artist(&art_name_p).await.unwrap();
    assert_eq!(art2["code"], 200);
    println!("kugou artist (by name): {} songs: {} albums: {}", art2["artist"]["name"], art2["songs"].as_array().unwrap().len(), art2["albums"].as_array().unwrap().len());
    assert!(art2["songs"].as_array().unwrap().len() > 0);
}

#[tokio::test]
async fn qq_live_lyric_decrypt() {
    let c = QqmusicClient::new(HashMap::new());
    let mut p = HashMap::new();
    p.insert("id".to_string(), serde_json::json!("97773")); // 晴天 songID
    p.insert("name".to_string(), serde_json::json!("晴天"));
    p.insert("artist".to_string(), serde_json::json!("周杰伦"));
    let res = c.lyric(&p).await.unwrap();
    assert_eq!(res["code"], 200);
    let qrc_or_lrc = res["qrc"].as_str().or_else(|| res["lrc"].as_str()).unwrap();
    println!("qq lyric decrypted sample (len={}): {}", qrc_or_lrc.len(), &qrc_or_lrc[..qrc_or_lrc.len().min(120)]);
    assert!(qrc_or_lrc.contains("晴天") || qrc_or_lrc.contains("周杰伦") || qrc_or_lrc.contains("00:"));
}

#[tokio::test]
async fn kugou_live_lyric_krc_decrypt() {
    let c = KugouClient::new(HashMap::new());
    let mut p = HashMap::new();
    p.insert("hash".to_string(), serde_json::json!("b3a52a7a958bf0aed0ebfba2e9a818b7")); // 晴天 hash
    p.insert("name".to_string(), serde_json::json!("晴天"));
    p.insert("duration".to_string(), serde_json::json!(269));
    let res = c.lyric(&p).await.unwrap();
    assert_eq!(res["code"], 200);
    let lrc = res["lrc"].as_str().unwrap();
    println!("kugou lyric decrypted sample (len={}): {}", lrc.len(), &lrc[..lrc.len().min(120)]);
    assert!(lrc.contains("晴天") || lrc.contains("周杰伦") || lrc.contains("00:"));
}






