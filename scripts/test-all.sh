#!/usr/bin/env bash
set -e

# ==============================================================================
# SPlayer-Next-Headless 纯 Headless 模式全自动化测试套件
# ==============================================================================

BOLD="\033[1m"
GREEN="\033[32m"
RED="\033[31m"
YELLOW="\033[33m"
CYAN="\033[36m"
RESET="\033[0m"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo -e "${BOLD}${CYAN}======================================================${RESET}"
echo -e "${BOLD}${CYAN}   SPlayer-Next-Headless 核心自动化测试套件           ${RESET}"
echo -e "${BOLD}${CYAN}======================================================${RESET}\n"

TOTAL_TESTS=6
PASSED_TESTS=0
FAILED_TESTS=0
FAILED_NAMES=()

run_test_step() {
    local step_num="$1"
    local step_name="$2"
    local test_cmd="$3"

    echo -e "${BOLD}${YELLOW}[${step_num}/${TOTAL_TESTS}] 正在运行: ${step_name}...${RESET}"
    echo -e "${CYAN}命令: ${test_cmd}${RESET}"

    local start_time
    start_time=$(date +%s)

    if eval "$test_cmd"; then
        local end_time
        end_time=$(date +%s)
        local duration=$((end_time - start_time))
        echo -e "${GREEN}✓ ${step_name} 测试通过 (耗时: ${duration}s)${RESET}\n"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        local end_time
        end_time=$(date +%s)
        local duration=$((end_time - start_time))
        echo -e "${RED}✗ ${step_name} 测试失败 (耗时: ${duration}s)${RESET}\n"
        FAILED_TESTS=$((FAILED_TESTS + 1))
        FAILED_NAMES+=("${step_name}")
    fi
}

# 1. 音频引擎核心库单元测试 (音频解码、重采样、DSP、Diretta、音量归一化)
run_test_step 1 "audio-engine-core 核心单元测试" "cargo test -p audio-engine-core --lib"

# 2. SACD ISO 专项解析与分轨测试 (ScarletBook / Master TOC / Area TOC / 虚拟分轨)
run_test_step 2 "SACD ISO 专项测试 (Master TOC / Area TOC / 虚拟分轨)" "cargo test -p audio-engine-core --test sacd_test"

# 3. CUE 分轨与智能匹配测试 (UTF-8 / GBK / BOM 编码探测与时间戳解析)
run_test_step 3 "CUE 分轨解析与模糊匹配测试" "cargo test -p audio-engine-core --test cue_parser_test"

# 4. DSD 解码器与比特流转换测试 (DSF / DFF / Dsd2Pcm Decimator / 位翻转 LUT)
run_test_step 4 "DSD 解码与 Dsd2Pcm 转换测试" "cargo test -p audio-engine-core --test dsd_decoder_test"

# 5. Headless 服务端全套集成测试 (Axum API / SQLite 数据库 / Diretta / 静态托管 / 歌单)
run_test_step 5 "Headless Server (API / 数据库 / Diretta / 播放器) 测试" "cargo test -p headless-server"

# 6. Headless 模式相关所有 Crate 聚合联调测试 (排除无关的桌面端 Windows/Electron 插件)
run_test_step 6 "Headless 全模块聚合测试 (Core + Server + Diretta + OpenCC)" "cargo test -p audio-engine-core -p headless-server -p diretta-sys -p opencc"

# ==============================================================================
# 测试汇总报告
# ==============================================================================
echo -e "${BOLD}${CYAN}======================================================${RESET}"
echo -e "${BOLD}测试运行总结:${RESET}"
echo -e "  总计测试项: ${TOTAL_TESTS}"
echo -e "  ${GREEN}成功: ${PASSED_TESTS}${RESET}"
if [ $FAILED_TESTS -gt 0 ]; then
    echo -e "  ${RED}失败: ${FAILED_TESTS}${RESET}"
    echo -e "\n${RED}以下测试未通过:${RESET}"
    for name in "${FAILED_NAMES[@]}"; do
        echo -e "  - ${RED}${name}${RESET}"
    done
    echo -e "${BOLD}${CYAN}======================================================${RESET}\n"
    exit 1
else
    echo -e "  ${RED}失败: 0${RESET}"
    echo -e "\n${BOLD}${GREEN}🎉 恭喜！Headless 模式所有测试均已 100% 顺利通过！${RESET}"
    echo -e "${BOLD}${CYAN}======================================================${RESET}\n"
    exit 0
fi
