#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# SPlayer Headless 自适应 CPU 架构智能启动脚本
# 自动探测运行机 CPU 最高支持的指令集，无缝加载最高性能的二进制版本
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${SCRIPT_DIR}"

for candidate in \
    "${SCRIPT_DIR}/../dist/releases" \
    "${SCRIPT_DIR}/releases" \
    "/opt/splayer-headless/releases" \
    "/opt/splayer-headless" \
    "${SCRIPT_DIR}"; do
    if [ -d "${candidate}" ] && ls "${candidate}"/splayer-headless* >/dev/null 2>&1; then
        BIN_DIR="${candidate}"
        break
    fi
done

# 检查 CPU 特性
detect_best_binary() {
    local cpuinfo="/proc/cpuinfo"

    if [ ! -f "${cpuinfo}" ]; then
        # 兜底：若无法读取 cpuinfo，使用 v2
        echo "${BIN_DIR}/splayer-headless-x86_64-v2"
        return
    fi

    # 1. 检测 Zen 4 (znver4: 包含 avx512 且 vendor 是 AuthenticAMD / family 25 / 26)
    if grep -q "AuthenticAMD" "${cpuinfo}" && grep -q "avx512" "${cpuinfo}"; then
        if [ -f "${BIN_DIR}/splayer-headless-zen4" ]; then
            echo "${BIN_DIR}/splayer-headless-zen4"
            return
        fi
    fi

    # 2. 检测 x86-64-v4 (AVX-512)
    if grep -q "avx512f" "${cpuinfo}" || grep -q "avx512" "${cpuinfo}"; then
        if [ -f "${BIN_DIR}/splayer-headless-x86_64-v4" ]; then
            echo "${BIN_DIR}/splayer-headless-x86_64-v4"
            return
        fi
    fi

    # 3. 检测 x86-64-v3 (AVX2 + BMI2 + FMA)
    if grep -q "avx2" "${cpuinfo}" && grep -q "bmi2" "${cpuinfo}" && grep -q "fma" "${cpuinfo}"; then
        if [ -f "${BIN_DIR}/splayer-headless-x86_64-v3" ]; then
            echo "${BIN_DIR}/splayer-headless-x86_64-v3"
            return
        fi
    fi

    # 4. 默认兜底 x86-64-v2
    if [ -f "${BIN_DIR}/splayer-headless-x86_64-v2" ]; then
        echo "${BIN_DIR}/splayer-headless-x86_64-v2"
        return
    fi

    # 5. 单一标准构建二进制
    echo "${BIN_DIR}/splayer-headless"
}

BEST_BIN=$(detect_best_binary)

if [ ! -f "${BEST_BIN}" ]; then
    echo "[Error] 找不到可执行的 SPlayer Headless 二进制文件: ${BEST_BIN}" >&2
    exit 1
fi

chmod +x "${BEST_BIN}" 2>/dev/null || true
echo "[SPlayer Launcher] 探测到最优 CPU 架构版本: $(basename "${BEST_BIN}")"
exec "${BEST_BIN}" "$@"
