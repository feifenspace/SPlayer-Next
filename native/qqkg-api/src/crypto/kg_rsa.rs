//! 酷狗无填充 RSA 加密（对齐桌面端 crypto.ts 的 rsaEncryptKugou）。

use num_bigint_dig::BigUint;
use num_traits::Num;

/// RSA-1024 公钥模数（十六进制，从桌面端 PEM SPKI 提取，指数 65537）。
/// 模数正确性由对照单测间接锁定：若与桌面端密钥不符，密文向量必然不匹配。
const KG_RSA_N_HEX: &str = "c8006ed03842d2628209bd314984ca5ed6cfe06e30c95f9d4704d9c49791d7a935ba950ecb0bc8ebf5f5994f0bac927a7eb151b3c1de343303fa539c83136eccfd7d7e511e2dbce18eaa9f784c9b50d443e75865979e0a5e216e46c684066a8d6b998580bbaa22d73f5790286bb14742e83244e44db6d707ffe162c5c7002d45";

const RSA_E: u64 = 65537;
const BLOCK_LEN: usize = 128;

/// 按 KG 协议执行无填充 RSA 加密。
///
/// 行为逐字节对齐桌面端 `rsaEncryptKugou`：
/// - 明文 UTF-8 字节**左对齐**零填充至 128 字节（node `Buffer.copy` 语义，
///   与标准 no-padding 的右对齐惯例相反）
/// - 超过 128 字节的输入**静默截断**至前 128 字节（同样是 node `Buffer.copy`
///   语义；实际调用方 payload 应远小于此，仅保证行为与桌面端一致）
/// - c = m^e mod n
/// - 输出**小写** hex、固定 256 字符（保留密文前导零）；
///   user_detail 的 `p` 参数需调用方 `to_uppercase()`
///
/// 注：模数最高字节 0xC8 > '{'(0x7B)，JSON 明文恒小于模数，不存在回绕。
pub fn rsa_encrypt_kugou(input_json: &str) -> String {
    let n = BigUint::from_str_radix(KG_RSA_N_HEX, 16).expect("KG RSA modulus is valid hex");
    let e = BigUint::from(RSA_E);

    let bytes = input_json.as_bytes();
    let copy_len = bytes.len().min(BLOCK_LEN);
    let mut block = [0u8; BLOCK_LEN];
    block[..copy_len].copy_from_slice(&bytes[..copy_len]);
    let m = BigUint::from_bytes_be(&block);
    let c = m.modpow(&e, &n);

    // 密文右对齐补前导零至 128 字节，保证 hex 恒 256 字符（与 node 输出一致）
    let c_bytes = c.to_bytes_be();
    let mut out = [0u8; BLOCK_LEN];
    out[BLOCK_LEN - c_bytes.len()..].copy_from_slice(&c_bytes);
    hex_lower(&out)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // 输入 JSON 与期望密文均由桌面端算法的 node 复刻版生成（同一密钥、同一 JSON 串）
    #[test]
    fn rsa_matches_desktop_token_vector() {
        let input = r#"{"token":"test-token-abc123","clienttime":1740000000}"#;
        assert_eq!(
            rsa_encrypt_kugou(input),
            "93743a42e2b5544b3a6b43b637697fe47c28e45c079364cecf2c93a5804db2158ecd942129e8932ff9b2f2f4bbe454d3765d8fcfb2f7ea4281f108f216b84e58747ec288c469342356a96b25ea5f6fafa8ab454aa45d887615391f6fdd93558e7c6341aa4fd1eb45db98223c85b8e89204b61a8cd1f9341a58bd1fb68ce5064a"
        );
    }

    #[test]
    fn rsa_matches_desktop_long_vector() {
        let input = r#"{"token":"a-much-longer-token-value-to-exercise-more-bytes-0123456789","userid":"12345678","clienttime":1740000000,"musicid":"abcdef"}"#;
        assert_eq!(
            rsa_encrypt_kugou(input),
            "a4662915f2dce3b450a9126af571c51f0f7aba4de14333f17488acb676501bc4c4e2ccc0e1e99428d61bef070b0417677d2d3f6ada5ce99415b91f26eef34a501913c5f4f0c28fdeefc4d2dc189cee8d49b457e277b4c8d7b7a044a057d58b0d461bc7f6d544e4b6aef7a6cc43a3470c90d04e714c30332905d33bc92e29893d"
        );
    }

    #[test]
    fn rsa_output_is_fixed_256_lowercase_hex() {
        let out = rsa_encrypt_kugou(r#"{"a":1}"#);
        assert_eq!(out.len(), 256);
        assert!(out.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
