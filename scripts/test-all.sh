#!/usr/bin/env bash
set -e

# ==============================================================================
# SPlayer-Next-Headless 纯 Headless 模式全功能自动化测试套件
# ==============================================================================

BOLD="\033[1m"
GREEN="\033[32m"
RED="\033[31m"
YELLOW="\033[33m"
CYAN="\033[36m"
MAGENTA="\033[35m"
RESET="\033[0m"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.."; pwd)"
cd "$ROOT_DIR"

show_help() {
    echo -e "${BOLD}${CYAN}================================================================${RESET}"
    echo -e "${BOLD}${CYAN}   SPlayer-Next-Headless 全功能单项与模块化测试套件             ${RESET}"
    echo -e "${BOLD}${CYAN}================================================================${RESET}"
    echo -e "用法:"
    echo -e "  ./test.sh [选项/单项序号/别名...]"
    echo -e ""
    echo -e "${BOLD}1. 音频解码与 HiFi 引擎 (audio-engine-core):${RESET}"
    echo -e "  ${YELLOW} 1${RESET} | ${CYAN}core${RESET}       - 音频引擎核心库基础测试 (解码/重采样/DSP/音量均衡)"
    echo -e "  ${YELLOW} 2${RESET} | ${CYAN}sacd${RESET}       - SACD ISO 专项解析与分轨测试 (ScarletBook / TOC)"
    echo -e "  ${YELLOW} 3${RESET} | ${CYAN}cue${RESET}        - CUE 分轨解析与模糊编码探测测试"
    echo -e "  ${YELLOW} 4${RESET} | ${CYAN}dsd${RESET}        - DSD 解码器与 Dsd2Pcm 比特流转换测试"
    echo -e "  ${YELLOW} 5${RESET} | ${CYAN}scanner${RESET}    - 本地音乐库多线程扫描与元数据提取测试"
    echo -e "  ${YELLOW} 6${RESET} | ${CYAN}hifi${RESET}       - HiFi 格式测试 (MQA / HDCD / DTS)"
    echo -e "  ${YELLOW}16${RESET} | ${CYAN}ram${RESET}        - 纯内存 RAM Play 双缓冲 + CPU 亲和力调度测试"
    echo -e ""
    echo -e "${BOLD}2. Headless 服务端与网络引擎 (headless-server):${RESET}"
    echo -e "  ${YELLOW} 7${RESET} | ${CYAN}db${RESET}         - SQLite 本地曲库与统计测试 (Tracks/Albums/Artists)"
    echo -e "  ${YELLOW} 8${RESET} | ${CYAN}api${RESET}        - Headless REST API 与播放器服务端测试"
    echo -e "  ${YELLOW} 9${RESET} | ${CYAN}playlist${RESET}   - 歌单持久化存储与配置测试"
    echo -e "  ${YELLOW}10${RESET} | ${CYAN}static${RESET}     - Web 前端静态托管与 SPA 路由测试"
    echo -e "  ${YELLOW}11${RESET} | ${CYAN}diretta${RESET}    - Diretta 专网音频传输与守护进程测试"
    echo -e "  ${YELLOW}12${RESET} | ${CYAN}opencc${RESET}     - OpenCC 简繁中文转换与歌词分词测试"
    echo -e ""
    echo -e "${BOLD}3. Web 前端与流媒体模块 (Web / Vitest):${RESET}"
    echo -e "  ${YELLOW}13${RESET} | ${CYAN}stream${RESET}     - Web 端流媒体源 (Subsonic/Navidrome/Jellyfin/Emby) 专项测试"
    echo -e "  ${YELLOW}14${RESET} | ${CYAN}client${RESET}     - Web / Headless 客户端网络适配测试"
    echo -e "  ${YELLOW}15${RESET} | ${CYAN}web${RESET}        - Web 前端全量单元测试套件 (Vitest)"
    echo -e ""
    echo -e "${BOLD}4. 聚合/分类快捷测试:${RESET}"
    echo -e "  ${MAGENTA}engine${RESET}      - 运行所有音频底层解码与扫描测试 (1-6, 16)"
    echo -e "  ${MAGENTA}server${RESET}      - 运行所有 Headless 服务端测试 (7-12)"
    echo -e "  ${MAGENTA}rust${RESET}        - 运行所有 Rust 后端核心测试 (1-12, 16)"
    echo -e "  ${MAGENTA}all${RESET}         - 运行全套 16 项测试 (默认)"
    echo -e ""
    echo -e "${BOLD}常用示例:${RESET}"
    echo -e "  ./test.sh 16            # 仅测试 RAM Play 纯内存模块"
    echo -e "  ./test.sh ram           # 仅测试 RAM Play 纯内存模块"
    echo -e "  ./test.sh 4 16          # DSD + RAM Play 联合测试"
    echo -e "  ./test.sh engine        # 运行全部音频解码引擎测试"
    echo -e "  ./test.sh -i            # 打开交互式菜单"
    echo -e "${BOLD}${CYAN}================================================================${RESET}\n"
}

if [ "$1" = "-h" ] || [ "$1" = "--help" ] || [ "$1" = "help" ]; then
    show_help
    exit 0
fi

SELECTED_STEPS=()

# 如果没有传参数，或者显式传了 -i / --interactive / menu，默认弹出交互式选择菜单
if [ $# -eq 0 ] || [ "$1" = "-i" ] || [ "$1" = "--interactive" ] || [ "$1" = "menu" ]; then
    echo -e "${BOLD}${CYAN}================================================================${RESET}"
    echo -e "${BOLD}${CYAN}   SPlayer-Next-Headless 测试选择菜单                           ${RESET}"
    echo -e "${BOLD}${CYAN}================================================================${RESET}"
    echo -e "  ${BOLD}[音频解码引擎]${RESET}"
    echo -e "    ${YELLOW}[ 1]${RESET} audio-engine-core 核心测试        ${YELLOW}[ 2]${RESET} SACD ISO 专项解析测试"
    echo -e "    ${YELLOW}[ 3]${RESET} CUE 分轨与智能匹配测试            ${YELLOW}[ 4]${RESET} DSD 解码与 Dsd2Pcm 测试"
    echo -e "    ${YELLOW}[ 5]${RESET} 音乐库多线程扫描测试              ${YELLOW}[ 6]${RESET} MQA / HDCD / DTS 特性测试"
    echo -e "    ${YELLOW}[16]${RESET} RAM Play 纯内存双缓冲 + CPU 亲和力调度测试"
    echo -e "  ${BOLD}[服务端与网络]${RESET}"
    echo -e "    ${YELLOW}[ 7]${RESET} SQLite 曲库与检索测试             ${YELLOW}[ 8]${RESET} REST API 与播放器集成测试"
    echo -e "    ${YELLOW}[ 9]${RESET} 歌单存储与配置测试                ${YELLOW}[10]${RESET} Web 静态托管与 SPA 测试"
    echo -e "    ${YELLOW}[11]${RESET} Diretta 守护进程测试              ${YELLOW}[12]${RESET} OpenCC 简繁中文转换测试"
    echo -e "  ${BOLD}[前端与流媒体]${RESET}"
    echo -e "    ${YELLOW}[13]${RESET} Web 流媒体 (Subsonic/Jellyfin)     ${YELLOW}[14]${RESET} Web 客户端网络适配测试"
    echo -e "    ${YELLOW}[15]${RESET} Web 前端全量 Vitest 测试"
    echo -e "  ${BOLD}[快捷组合]${RESET}"
    echo -e "    ${MAGENTA}[ E]${RESET} 全部音频引擎测试 (1-6, 16)        ${MAGENTA}[ S]${RESET} 全部服务端测试 (7-12)"
    echo -e "    ${MAGENTA}[ R]${RESET} 全部 Rust 后端测试 (1-12, 16)      ${MAGENTA}[ A]${RESET} 运行全部 16 项测试 (默认)"
    echo -e "${BOLD}${CYAN}================================================================${RESET}"
    echo -ne "${BOLD}请输入选择 [1-16/E/S/R/A] (支持多个如 7 13，回车默认全选): ${RESET}"
    read -r user_input
    if [ -z "$user_input" ] || [ "$user_input" = "A" ] || [ "$user_input" = "a" ] || [ "$user_input" = "all" ]; then
        SELECTED_STEPS=("1" "2" "3" "4" "5" "6" "7" "8" "9" "10" "11" "12" "13" "14" "15" "16")
    elif [ "$user_input" = "E" ] || [ "$user_input" = "e" ] || [ "$user_input" = "engine" ]; then
        SELECTED_STEPS=("1" "2" "3" "4" "5" "6" "16")
    elif [ "$user_input" = "S" ] || [ "$user_input" = "s" ] || [ "$user_input" = "server" ]; then
        SELECTED_STEPS=("7" "8" "9" "10" "11" "12")
    elif [ "$user_input" = "R" ] || [ "$user_input" = "r" ] || [ "$user_input" = "rust" ]; then
        SELECTED_STEPS=("1" "2" "3" "4" "5" "6" "7" "8" "9" "10" "11" "12" "16")
    else
        SELECTED_STEPS=($user_input)
    fi
else
    for arg in "$@"; do
        case "$arg" in
            1|core|audio-core) SELECTED_STEPS+=("1") ;;
            2|sacd|iso) SELECTED_STEPS+=("2") ;;
            3|cue) SELECTED_STEPS+=("3") ;;
            4|dsd|dsf|dff) SELECTED_STEPS+=("4") ;;
            5|scanner|scan) SELECTED_STEPS+=("5") ;;
            6|hifi|mqa|hdcd|dts) SELECTED_STEPS+=("6") ;;
            7|db|database|sqlite) SELECTED_STEPS+=("7") ;;
            8|api|http|rest) SELECTED_STEPS+=("8") ;;
            9|playlist|playlists) SELECTED_STEPS+=("9") ;;
            10|static|spa|web-host) SELECTED_STEPS+=("10") ;;
            11|diretta|diretta-sys) SELECTED_STEPS+=("11") ;;
            12|opencc|chinese) SELECTED_STEPS+=("12") ;;
            13|stream|streaming|subsonic|navidrome|jellyfin) SELECTED_STEPS+=("13") ;;
            14|client|polyfill) SELECTED_STEPS+=("14") ;;
            15|web|frontend|vitest) SELECTED_STEPS+=("15") ;;
            16|ram|ram-play|rambuffer|memory) SELECTED_STEPS+=("16") ;;
            engine|audio) SELECTED_STEPS+=("1" "2" "3" "4" "5" "6" "16") ;;
            server|headless|backend) SELECTED_STEPS+=("7" "8" "9" "10" "11" "12") ;;
            rust|rust-all|crates) SELECTED_STEPS+=("1" "2" "3" "4" "5" "6" "7" "8" "9" "10" "11" "12" "16") ;;
            all|a) SELECTED_STEPS=("1" "2" "3" "4" "5" "6" "7" "8" "9" "10" "11" "12" "13" "14" "15" "16"); break ;;
            *)
                echo -e "${RED}未知测试参数: ${arg}${RESET}"
                show_help
                exit 1
                ;;
        esac
    done
fi

echo -e "${BOLD}${CYAN}================================================================${RESET}"
echo -e "${BOLD}${CYAN}   SPlayer-Next-Headless 自动化测试开始执行                     ${RESET}"
echo -e "${BOLD}${CYAN}================================================================${RESET}\n"

TOTAL_SELECTED=${#SELECTED_STEPS[@]}
PASSED_TESTS=0
FAILED_TESTS=0
FAILED_NAMES=()
CURRENT_INDEX=0

run_test_step() {
    local step_num="$1"
    local step_name="$2"
    local test_cmd="$3"

    CURRENT_INDEX=$((CURRENT_INDEX + 1))
    echo -e "${BOLD}${YELLOW}[${CURRENT_INDEX}/${TOTAL_SELECTED}] 正在运行: ${step_name}...${RESET}"
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

should_run() {
    local target="$1"
    for s in "${SELECTED_STEPS[@]}"; do
        if [ "$s" = "$target" ]; then
            return 0
        fi
    done
    return 1
}

# 1. 音频引擎核心库基础测试
if should_run "1"; then
    run_test_step 1 "audio-engine-core 核心库测试" "cargo test -p audio-engine-core --lib"
fi

# 2. SACD ISO 专项测试
if should_run "2"; then
    run_test_step 2 "SACD ISO 专项解析测试 (Master TOC / Area TOC)" "cargo test -p audio-engine-core --test sacd_test"
fi

# 3. CUE 分轨与智能匹配测试
if should_run "3"; then
    run_test_step 3 "CUE 分轨解析与模糊匹配测试" "cargo test -p audio-engine-core --test cue_parser_test"
fi

# 4. DSD 解码器与比特流转换测试
if should_run "4"; then
    run_test_step 4 "DSD 解码与 Dsd2Pcm 转换测试" "cargo test -p audio-engine-core --test dsd_decoder_test"
fi

# 5. 音乐库扫描与元数据提取测试
if should_run "5"; then
    run_test_step 5 "音乐库多线程扫描与元数据提取测试" "cargo test -p audio-engine-core --test scanner_integration"
fi

# 6. MQA / HDCD / DTS 特殊格式测试
if should_run "6"; then
    run_test_step 6 "HiFi 格式测试 (MQA / HDCD / DTS)" "cargo test -p audio-engine-core --test mqa_test --test hdcd_test --test dts_test"
fi

# 7. SQLite 数据库曲库测试
if should_run "7"; then
    run_test_step 7 "SQLite 本地曲库与检索测试" "cargo test -p headless-server --test library_db_test"
fi

# 8. Headless REST API 与播放器集成测试
if should_run "8"; then
    run_test_step 8 "Headless REST API 与播放器服务端测试" "cargo test -p headless-server --test api_integration_test"
fi

# 9. 歌单存储与配置测试
if should_run "9"; then
    run_test_step 9 "歌单持久化存储与配置测试" "cargo test -p headless-server --test playlist_config_test"
fi

# 10. Web 静态托管与 SPA 路由测试
if should_run "10"; then
    run_test_step 10 "Web 前端静态托管与 SPA 路由测试" "cargo test -p headless-server --test static_hosting_test"
fi

# 11. Diretta 守护进程与网络传输测试
if should_run "11"; then
    run_test_step 11 "Diretta 专网音频传输与守护进程测试" "cargo test -p audio-engine-core --lib diretta -- --nocapture && cargo test -p headless-server --test diretta_integration_test -p diretta-sys"
fi

# 12. OpenCC 简繁中文转换测试
if should_run "12"; then
    run_test_step 12 "OpenCC 简繁中文转换与歌词分词测试" "cargo test -p opencc"
fi

# 13. Web 流媒体模块专项测试
if should_run "13"; then
    run_test_step 13 "Web 流媒体 (Subsonic/Jellyfin/MD5/Search) 专项测试" "npx vitest run src/services/streaming/web/streaming.spec.ts"
fi

# 14. Web 客户端网络适配测试
if should_run "14"; then
    run_test_step 14 "Web / Headless 客户端网络适配测试" "npx vitest run src/services/client/client.spec.ts"
fi

# 15. Web 前端全量单元测试
if should_run "15"; then
    run_test_step 15 "Web 前端全量单元测试 (Vitest)" "pnpm test:web"
fi

# 16. 纯内存 RAM Play 双缓冲 + CPU 亲和力调度测试
# 测试内容：
#   - RamTrackBuffer：append/read/advance/fully_loaded/is_at_eof 生命周期
#   - RamPlayManager：30 秒 Gapless 预加载触发 + 原子指针交换（SwapToNext）
#   - DSD 静音字节（0x69）缓冲区初始化验证
#   - Linux CPU 亲和力（detect_performance_cores）与 SCHED_FIFO 设置（编译验证）
if should_run "16"; then
    run_test_step 16 "纯内存 RAM Play 双缓冲 + CPU 亲和力调度测试" \
        "cargo test -p audio-engine-core --lib ram_buffer -- --nocapture && cargo test -p audio-engine-core --lib priority -- --nocapture"
fi

# ==============================================================================
# 测试汇总报告
# ==============================================================================
echo -e "${BOLD}${CYAN}================================================================${RESET}"
echo -e "${BOLD}测试运行总结:${RESET}"
echo -e "  已执行测试项: ${TOTAL_SELECTED}"
echo -e "  ${GREEN}成功: ${PASSED_TESTS}${RESET}"
if [ $FAILED_TESTS -gt 0 ]; then
    echo -e "  ${RED}失败: ${FAILED_TESTS}${RESET}"
    echo -e "\n${RED}以下测试未通过:${RESET}"
    for name in "${FAILED_NAMES[@]}"; do
        echo -e "  - ${RED}${name}${RESET}"
    done
    echo -e "${BOLD}${CYAN}================================================================${RESET}\n"
    exit 1
else
    echo -e "  ${RED}失败: 0${RESET}"
    echo -e "\n${BOLD}${GREEN}🎉 恭喜！所选测试项目均已 100% 顺利通过！${RESET}"
    echo -e "${BOLD}${CYAN}================================================================${RESET}\n"
    exit 0
fi
