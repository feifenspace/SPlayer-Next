//! 酷狗签名算法（对齐桌面端 electron/main/apis/kugou/core/crypto.ts）。
//!
//! 各算法的排序规则不同，均按桌面端原样复刻，勿"统一"：
//! - `signature_web_params`：先拼成 `k=v` 字符串、再对**字符串**排序
//!   （前缀键如 dfid/dfid2 会得到与按键名排序不同的顺序，单测已锁定）
//! - `signature_android_params`：先按 key 排序、再拼 `k=v`
//! - `sign_key` / `sign_params_key`：无排序，纯拼接

/// Android 版签名盐值。
pub const ANDROID_SIGN_SALT: &str = "OIlwieks28dk2k092lksi2UIkp";
/// Web 版签名盐值。
pub const WEB_SIGN_SALT: &str = "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt";
/// song_url 的 key 签名盐值。
pub const SIGN_KEY_SALT: &str = "57ae12eb6890223e355ccfcb74edf70d";

fn md5_hex(input: &str) -> String {
    format!("{:x}", md5::compute(input.as_bytes()))
}

/// Web 版 API 请求签名：MD5(salt + 排序后的 k=v 拼接 + salt)。
pub fn signature_web_params(params: &[(String, String)]) -> String {
    let mut pairs: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    md5_hex(&format!("{}{}{}", WEB_SIGN_SALT, pairs.join(""), WEB_SIGN_SALT))
}

/// Android 版 API 请求签名：MD5(salt + 按键名排序的 k=v 拼接 + body + salt)。
pub fn signature_android_params(params: &[(String, String)], body: &str) -> String {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let params_string: String = sorted.iter().map(|(k, v)| format!("{k}={v}")).collect();
    md5_hex(&format!(
        "{}{}{}{}",
        ANDROID_SIGN_SALT, params_string, body, ANDROID_SIGN_SALT
    ))
}

/// song_url 的 key 签名：MD5(hash + key盐 + appid + mid + userid)。
pub fn sign_key(hash: &str, mid: &str, userid: &str, appid: u32) -> String {
    md5_hex(&format!("{}{}{}{}{}", hash, SIGN_KEY_SALT, appid, mid, userid))
}

/// signParamsKey：MD5(appid + android盐 + clientver + data)。
pub fn sign_params_key(data: &str, appid: u32, clientver: u32) -> String {
    md5_hex(&format!("{}{}{}{}", appid, ANDROID_SIGN_SALT, clientver, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 期望值由桌面端算法的 node 复刻版生成（m0 向量脚本），锁死两侧行为逐字节一致
    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn web_signature_typical() {
        let params = p(&[
            ("dfid", "-"),
            ("mid", "3621685531"),
            ("uuid", "8df6c1c2-3b1a-4e5f-9a7b-c8d9e0f1a2b3"),
            ("appid", "1005"),
            ("clientver", "20489"),
            ("clienttime", "1740000000"),
            ("plat", "4"),
            ("srcappid", "2919"),
            ("qrcode_txt", "https://h5.kugou.com/apps/loginQRCode/html/index.html?appid=1005&"),
        ]);
        assert_eq!(signature_web_params(&params), "fd9eec5129b05f8bbdbfdc8e3ffc2b94");
    }

    #[test]
    fn web_signature_sorts_kv_strings_not_keys() {
        // 锁定"先拼 k=v 再排序"的桌面端行为：'2'(0x32) < '='(0x3D)，
        // 故 "dfid2=x" 排在 "dfid=-" 之前；若误实现为按 key 排序将得到不同签名
        let params = p(&[("dfid", "-"), ("dfid2", "x")]);
        assert_eq!(signature_web_params(&params), "b047bd78e9316e0ab77749fcc91193d1");
    }

    #[test]
    fn android_signature_typical() {
        let params = p(&[
            ("appid", "1005"),
            ("clientver", "20489"),
            ("mid", "3621685531"),
            ("uuid", "8df6c1c2-3b1a-4e5f-9a7b-c8d9e0f1a2b3"),
            ("clienttime", "1740000000"),
        ]);
        assert_eq!(signature_android_params(&params, ""), "191e2e4b4aa2a09b20309c7d53764516");
    }

    #[test]
    fn sign_key_vectors() {
        assert_eq!(
            sign_key("abcdef0123456789abcdef0123456789", "3621685531", "0", 1005),
            "1deb897616cb99d768144881d9bebbbf"
        );
        assert_eq!(
            sign_key("abcdef0123456789abcdef0123456789", "3621685531", "12345678", 1005),
            "3eee48d78b6722f841c636ad22dc5a65"
        );
    }

    #[test]
    fn sign_params_key_vectors() {
        assert_eq!(sign_params_key("1740000000", 1005, 20489), "c0e11c6267b374448c2b35f36c208de0");
        assert_eq!(sign_params_key("abcdef0123456789", 1005, 20489), "0aeaa88538c3280b9392219ad324e085");
    }
}
