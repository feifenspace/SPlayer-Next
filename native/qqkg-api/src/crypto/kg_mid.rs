//! 酷狗设备 MID 计算（对齐桌面端 crypto.ts 的 calculateMid / getDeviceMid）。

use std::sync::LazyLock;

/// 桌面端 getDeviceMid() 使用的固定设备指纹种子。
const DEVICE_MID_SEED: &str = "splayer-next-kugou-device-mid";

/// 桌面端 getDeviceMid() 的结果（单测以桌面端向量锁定，非循环推导）。
static DEVICE_MID: LazyLock<String> = LazyLock::new(|| calculate_mid(DEVICE_MID_SEED));

/// 根据 GUID 计算设备 MID：MD5 hex 视作 128 位无符号大整数转十进制。
///
/// MD5 恰为 128 位，`u128` 完整容纳（含最高位为 1 的情况），
/// 与桌面端 `BigInt("0x" + digest).toString(10)` 的无符号语义一致。
pub fn calculate_mid(guid: &str) -> String {
    let digest = format!("{:x}", md5::compute(guid.as_bytes()));
    u128::from_str_radix(&digest, 16).unwrap_or_default().to_string()
}

/// 默认设备 MID（对齐桌面端缓存值）。
pub fn device_mid() -> &'static str {
    &DEVICE_MID
}

#[cfg(test)]
mod tests {
    use super::*;

    // 期望值由桌面端算法的 node 复刻版生成（m0 向量脚本）
    #[test]
    fn calculate_mid_vectors() {
        // crypto.ts calculateMid() 的默认 GUID
        assert_eq!(
            calculate_mid("550e8400-e29b-41d4-a716-446655440000"),
            "308741372901437977228425563242293240765"
        );
        // getDeviceMid() 的实际种子 → 桌面端向量锁定
        assert_eq!(
            calculate_mid(DEVICE_MID_SEED),
            "313977252576711075203126132950927082200"
        );
        assert_eq!(device_mid(), "313977252576711075203126132950927082200");
        assert_eq!(
            calculate_mid("550e8400-e29b-41d4-a716-446655440001"),
            "339736486683836804256843102229530908313"
        );
    }
}
