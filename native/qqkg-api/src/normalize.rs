//! 规范化层的纯函数与 JSON 值提取辅助。

use serde_json::Value;

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

/// 从 JSON 对象数组（`[{"name": "周杰伦"}]`）中拼接歌手名。
pub fn join_singers(singers: &[Value]) -> String {
    singers
        .iter()
        .filter_map(|s| s.get("name").or_else(|| s.get("title")).and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// HTML 实体反转义（对齐桌面端 kugou/core/config.ts 的 decodeName）。
pub fn decode_name(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#039;", "'")
}

/// 填充酷狗封面模板的 `{size}` 占位符并升级 https。
pub fn fill_cover(url: &str, size: u32) -> String {
    if url.is_empty() {
        return String::new();
    }
    secure_url(&url.replace("{size}", &size.to_string()))
}

/// 填充 Option 类型的酷狗封面模板。
pub fn fill_cover_opt(url: Option<&str>, size: u32) -> String {
    url.map(|u| fill_cover(u, size)).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// JSON 值提取辅助（模拟 JS 的宽松取值语义）
// ---------------------------------------------------------------------------

/// 取字符串字段；空串与缺失等价（JS `??` + 后续 `|| ""` 的合并行为）。
pub fn val_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// 取数字字段为 f64（酷狗 filesize 等可能超 u32，统一 f64 再按需转）。
pub fn val_f64(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or_default()
}

/// 取 u64 字段。
pub fn val_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or_default()
}

/// 取 i64 字段（可能为负）。
pub fn val_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or_default()
}

/// 字段转字符串，兼容 string / number 两种 JSON 形态（酷狗 album_id 等）。
pub fn val_str_or_num(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(n) if n.is_number() => Some(n.to_string()),
        _ => None,
    }
}

/// JS 真值语义下的字符串提取：仅非空字符串返回 Some。
pub fn val_str_truthy(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// JS 真值语义下的数字提取：非零数字返回 Some。
pub fn val_num_truthy(v: &Value, key: &str) -> Option<f64> {
    v.get(key)
        .and_then(Value::as_f64)
        .filter(|n| *n != 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn join_singers_from_json() {
        let arr = vec![json!({"name": "周杰伦"}), json!({"name": "阿信"})];
        assert_eq!(join_singers(&arr), "周杰伦 / 阿信");
    }

    #[test]
    fn decode_name_entities() {
        assert_eq!(decode_name("A&nbsp;&amp;&nbsp;B"), "A & B");
        assert_eq!(decode_name("&lt;em&gt;X&lt;/em&gt;"), "<em>X</em>");
        assert_eq!(decode_name("&#039;quote&apos;"), "'quote'");
    }

    #[test]
    fn fill_cover_size_and_https() {
        assert_eq!(
            fill_cover("http://imge.kugou.com/temple/{size}/a.jpg", 300),
            "https://imge.kugou.com/temple/300/a.jpg"
        );
        assert_eq!(
            fill_cover_opt(Some("http://imge.kugou.com/temple/{size}/a.jpg"), 300),
            "https://imge.kugou.com/temple/300/a.jpg"
        );
        assert_eq!(fill_cover_opt(None, 300), "");
    }

    #[test]
    fn val_helpers_js_semantics() {
        let v = json!({ "a": "x", "b": "", "c": 5, "d": 0, "e": "123", "f": null });
        assert_eq!(val_str(&v, "a"), "x");
        assert_eq!(val_str(&v, "b"), "");
        assert_eq!(val_str(&v, "zz"), "");
        assert_eq!(val_u64(&v, "c"), 5);
        assert_eq!(val_f64(&v, "d"), 0.0);
        assert_eq!(val_str_or_num(&v, "e").as_deref(), Some("123"));
        assert_eq!(val_str_or_num(&v, "c").as_deref(), Some("5"));
        assert_eq!(val_str_or_num(&v, "f"), None);
        assert_eq!(val_str_truthy(&v, "b"), None);
        assert_eq!(val_str_truthy(&v, "a").as_deref(), Some("x"));
        assert_eq!(val_num_truthy(&v, "d"), None);
        assert_eq!(val_num_truthy(&v, "c"), Some(5.0));
    }
}
