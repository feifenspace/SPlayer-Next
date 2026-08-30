//! 规范化层的纯函数（信封构造与字段映射在 M1 随真实 fixture 一起补齐）。

/// 去除搜索命中词的高亮标签（QQ/酷狗部分字段带 `<em>` 包裹）。
pub fn strip_highlight(s: &str) -> String {
    s.replace("<em>", "").replace("</em>", "")
}

/// 封面 URL 强制 https（web 端混合内容拦截）。
pub fn secure_url(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

/// 多歌手名拼接。
pub fn join_artists(artists: &[String]) -> String {
    artists.join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_highlight_removes_em_tags() {
        assert_eq!(strip_highlight("周杰伦<em>晴天</em>"), "周杰伦晴天");
        assert_eq!(strip_highlight("plain"), "plain");
        assert_eq!(strip_highlight(""), "");
    }

    #[test]
    fn secure_url_upgrades_http() {
        assert_eq!(secure_url("http://a.com/x.jpg"), "https://a.com/x.jpg");
        assert_eq!(secure_url("https://a.com/x.jpg"), "https://a.com/x.jpg");
        assert_eq!(secure_url("//a.com/x.jpg"), "//a.com/x.jpg");
    }

    #[test]
    fn join_artists_sep() {
        assert_eq!(join_artists(&["A".into(), "B".into()]), "A / B");
        assert_eq!(join_artists(&[]), "");
    }
}
