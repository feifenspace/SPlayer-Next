#!/usr/bin/env bash
set -Eeuo pipefail
#
# SPlayer Linux Headless 一键编译、安装、部署脚本
#
# 用法：
#   sudo ./scripts/install-linux-headless.sh
#   sudo ./scripts/install-linux-headless.sh --sdk-version 149
#   sudo ./scripts/install-linux-headless.sh --sdk-dir /opt/DirettaHostSDK_148
#   sudo ./scripts/install-linux-headless.sh --skip-web
#   sudo ./scripts/install-linux-headless.sh --skip-build
#   sudo ./scripts/install-linux-headless.sh --no-start
#   sudo ./scripts/install-linux-headless.sh --no-clean
#   sudo ./scripts/install-linux-headless.sh --update
#   sudo ./scripts/install-linux-headless.sh --uninstall
#   sudo ./scripts/install-linux-headless.sh --uninstall --purge
#   sudo ./scripts/install-linux-headless.sh --uninstall --purge --yes
#
# 默认（不带 --uninstall）为全新安装或就地更新：保留现有配置与数据，
# 安装时自动清理旧版本程序与前端文件后重新部署（不再生成 .bak 备份）。
# --update 为显式更新模式，行为与默认安装一致。
# --uninstall 停止并移除服务与程序；默认保留配置与数据，--purge 连同删除（含数据库），
# --purge 删除前需交互输入 yes 确认，--yes 跳过确认供脚本/自动化使用。
#
# 无参数且在交互终端运行时显示数字菜单，按提示选择安装/更新/卸载功能；
# 选择安装/更新时会先检测旧版本部署（服务/程序/前端文件/配置）并提示处理方式；
# 若发现多个 Diretta SDK 版本会提示选择，也可用 DIRETTA_SDK_DIR 预先指定；
# 带参数运行为脚本化直接执行（行为不变）；无参数且非交互时默认执行安装。
#
# 默认会停止并清理已知旧版服务和 SPlayer Headless 进程；--no-clean 可跳过。
# --skip-build 跳过编译，直接使用仓库内现有编译产物安装（需已执行过一次完整安装编译，
# 适合仅改配置/服务文件或重装场景；菜单模式检测到已有产物时也会询问是否跳过）。
# 可选环境变量：
#   DIRETTA_SDK_DIR  DirettaHostSDK 根目录（等同 --sdk-dir）
#   SPLAYER_SDK_BASE DirettaHostSDK_* 搜索根目录，默认原调用用户 home
#   SPLAYER_USER     运行服务的 Linux 用户，默认当前调用 sudo 的用户或 splayer
#   SPLAYER_PORT     HTTP 端口，默认 14558
#   SPLAYER_DATA_DIR 数据目录，默认 /var/lib/splayer-headless
#

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="/opt/splayer-headless"
BIN_PATH="${INSTALL_DIR}/splayer-headless"
WEB_DIR="${INSTALL_DIR}/web"
CONFIG_DIR="${INSTALL_DIR}/config"
CONFIG_PATH="${CONFIG_DIR}/config.yaml"
DATA_DIR="${SPLAYER_DATA_DIR:-/var/lib/splayer-headless}"
SERVICE_NAME="splayer-headless.service"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}"
PORT="${SPLAYER_PORT:-14558}"
SDK_DIR="${DIRETTA_SDK_DIR:-}"
SDK_VERSION=""
SKIP_WEB=0
SKIP_BUILD=0
NO_START=0
CLEAN_OLD=1
UNINSTALL=0
PURGE=0
UPDATE=0
ASSUME_YES=0

# 仅清理已知的 SPlayer Headless 服务和进程，不使用宽泛的 pkill，避免误伤其他程序。
OLD_SERVICE_NAMES=(
    "splayer-headless.service"
    "splayer.service"
    "splayer-headless-server.service"
)

log() { printf '[splayer-headless] %s\n' "$*"; }
warn() { printf '[splayer-headless][WARN] %s\n' "$*" >&2; }
fatal() { printf '[splayer-headless][ERROR] %s\n' "$*" >&2; exit 1; }

usage() {
    # 打印文件头部帮助块：shebang + set 行 + 其后连续注释，遇代码即停，
    # 增删头注释无需同步行号
    awk 'NR <= 2 { print; next } /^#/ { print; next } { exit }' "$0"
}

# 交互式数字菜单：无参数且在交互终端运行时出现，选择后设置对应模式标志，
# 随后走统一主流程（sudo 检查、purge 确认等安全机制全部保留）
show_menu() {
    echo
    echo "========== SPlayer Linux Headless 管理菜单 =========="
    echo "  1) 安装 / 就地更新（保留配置与数据库）"
    echo "  2) 卸载（保留配置与数据库）"
    echo "  3) 卸载并清除全部数据（含数据库，需二次确认）"
    echo "  0) 退出"
    local choice=""
    while true; do
        read -r -p "请选择功能 [0-3]: " choice || fatal "读取选择失败"
        case "$choice" in
            1) UPDATE=1; check_existing_for_menu; maybe_skip_build_in_menu; choose_sdk_in_menu; break ;;
            2) UNINSTALL=1; break ;;
            3) UNINSTALL=1; PURGE=1; break ;;
            0) exit 0 ;;
            *) warn "无效选择：$choice，请输入 0-3" ;;
        esac
    done
    echo
}

# 菜单模式的 SDK 选择：按与主流程相同的规则发现 DirettaHostSDK_*；
# 已用 DIRETTA_SDK_DIR 显式指定、仅发现一个或未发现时跳过，交由主流程处理
choose_sdk_in_menu() {
    if [[ -n "$SDK_DIR" ]]; then
        log "已通过 DIRETTA_SDK_DIR 指定 SDK：$SDK_DIR"
        return 0
    fi

    discover_sdks
    if [[ ${#SDK_LIST[@]} -eq 0 ]]; then
        warn "未发现 DirettaHostSDK_*（搜索根：$SDK_SEARCH_BASE），安装时将按主流程规则处理"
        return 0
    fi
    if [[ ${#SDK_LIST[@]} -eq 1 ]]; then
        SDK_DIR="${SDK_LIST[0]}"
        log "自动选择唯一可用的 Diretta SDK：$SDK_DIR"
        return 0
    fi

    echo
    echo "---------- 选择 Diretta SDK（发现于 $SDK_SEARCH_BASE） ----------"
    local i=1 d choice
    for d in "${SDK_LIST[@]}"; do
        echo "  $i) $(basename "$d")"
        i=$((i + 1))
    done
    echo "  0) 不指定，由主流程自动检测"
    while true; do
        read -r -p "请选择 SDK [0-${#SDK_LIST[@]}]: " choice || fatal "读取选择失败"
        if [[ "$choice" == "0" ]]; then
            return 0
        elif [[ "$choice" =~ ^[0-9]+$ ]] && (( 10#$choice >= 1 && 10#$choice <= ${#SDK_LIST[@]} )); then
            SDK_DIR="${SDK_LIST[$((10#$choice - 1))]}"
            log "已选择 Diretta SDK：$SDK_DIR"
            return 0
        fi
        warn "无效选择：$choice，请输入 0-${#SDK_LIST[@]}"
    done
}

# 检测旧版本部署痕迹：服务单元、程序、前端文件、配置。
# 结果写入 EXISTING_PARTS 数组；只读检测，不做任何删除动作，供菜单与主流程共用。
# 全部用 if 形式：set -e 下函数末尾的 `[[ ]] &&` 会让函数意外返回 1 中断主流程
detect_existing_install() {
    EXISTING_PARTS=()
    if [[ -f "$SERVICE_PATH" ]]; then
        EXISTING_PARTS+=("服务单元：$SERVICE_PATH")
    fi
    if [[ -f "$BIN_PATH" ]]; then
        EXISTING_PARTS+=("程序：$BIN_PATH")
    fi
    if [[ -d "$WEB_DIR" ]]; then
        EXISTING_PARTS+=("前端文件：$WEB_DIR")
    fi
    if [[ -f "$CONFIG_PATH" ]]; then
        EXISTING_PARTS+=("配置：$CONFIG_PATH")
    fi
}

# 菜单模式的旧版本检测：选择安装/更新后立即展示已存在的部署与本次处理方式
check_existing_for_menu() {
    detect_existing_install
    if [[ ${#EXISTING_PARTS[@]} -eq 0 ]]; then
        log "未检测到旧版本部署，将执行全新安装"
        return 0
    fi
    log "检测到旧版本部署，本次安装/更新将自动处理："
    local item
    for item in "${EXISTING_PARTS[@]}"; do
        echo "  - $item"
    done
    log "处理方式：停止旧服务并清理旧程序与前端文件后重新安装；配置与数据库保留"
    detect_db_files
    if [[ ${#DB_FILES[@]} -gt 0 ]]; then
        log "数据库将保留：${DB_FILES[*]}"
    fi
}

# 菜单模式的跳过编译询问：已有编译产物时让用户选择复用，避免重复编译耗时；
# 无产物时不询问直接进入编译。跳过编译时无需 SDK，后续 SDK 选择自动略过
maybe_skip_build_in_menu() {
    local have_server=0 have_web=0
    if [[ -x "$PROJECT_ROOT/target/release/headless-server" ]]; then
        have_server=1
    fi
    if [[ -f "$PROJECT_ROOT/out/renderer/index.html" ]]; then
        have_web=1
    fi
    if [[ $have_server -eq 0 && $have_web -eq 0 ]]; then
        log "未发现已有编译产物，将执行完整编译"
        return 0
    fi
    echo "发现已有编译产物："
    if [[ $have_server -eq 1 ]]; then
        echo "  - 服务端：$PROJECT_ROOT/target/release/headless-server"
    fi
    if [[ $have_web -eq 1 ]]; then
        echo "  - Web UI：$PROJECT_ROOT/out/renderer"
    fi
    local reply=""
    read -r -p "是否跳过编译，直接使用现有产物安装？[y/N]: " reply || reply=""
    case "$reply" in
        y|Y|yes|YES)
            SKIP_BUILD=1
            log "将跳过编译，使用现有产物安装"
            ;;
        *)
            log "将重新编译（Rust 代码未变化时 cargo 会自动复用缓存，仅 Web 构建为全量）"
            ;;
    esac
}

ARG_COUNT=$#
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sdk-dir)
            [[ $# -ge 2 ]] || fatal "--sdk-dir 需要参数"
            SDK_DIR="$2"
            shift 2
            ;;
        --sdk-version)
            [[ $# -ge 2 ]] || fatal "--sdk-version 需要参数（如 148/149/150）"
            SDK_VERSION="$2"
            shift 2
            ;;
        --skip-web)
            SKIP_WEB=1
            shift
            ;;
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        --no-start)
            NO_START=1
            shift
            ;;
        --no-clean)
            CLEAN_OLD=0
            shift
            ;;
        --update)
            UPDATE=1
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --purge)
            PURGE=1
            shift
            ;;
        --yes|-y)
            ASSUME_YES=1
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            fatal "未知参数：$1"
            ;;
    esac
done

setup_rust_path() {
    # sudo 默认不会继承普通用户的 ~/.cargo/bin，优先恢复调用 sudo 的用户的 Rust 工具链。
    local rust_user="${SUDO_USER:-${USER:-}}"
    local rust_home=""
    if [[ -n "$rust_user" ]] && command -v getent >/dev/null 2>&1; then
        rust_home="$(getent passwd "$rust_user" | cut -d: -f6)"
    fi

    # sudo 会把 HOME 改成 /root；显式指向原调用用户的 Rust 配置目录。
    if [[ -n "$rust_home" && -d "$rust_home/.rustup" ]]; then
        export RUSTUP_HOME="$rust_home/.rustup"
    fi
    if [[ -n "$rust_home" && -d "$rust_home/.cargo" ]]; then
        export CARGO_HOME="$rust_home/.cargo"
    fi

    for rust_bin in \
        "${rust_home:+$rust_home/.cargo/bin}" \
        "/root/.cargo/bin" \
        "/usr/local/cargo/bin"; do
        if [[ -n "$rust_bin" && -x "$rust_bin/cargo" ]]; then
            case ":$PATH:" in
                *":$rust_bin:"*) ;;
                *) PATH="$rust_bin:$PATH" ;;
            esac
            export PATH
            break
        fi
    done
}

setup_rust_toolchain() {
    # 若已加入 cargo 路径但仍无默认 toolchain，自动为当前调用者配置 stable。
    if command -v cargo >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then
        if ! rustup show active-toolchain >/dev/null 2>&1; then
            local rust_user="${SUDO_USER:-${USER:-}}"
            local rust_home=""
            if [[ -n "$rust_user" ]] && command -v getent >/dev/null 2>&1; then
                rust_home="$(getent passwd "$rust_user" | cut -d: -f6)"
            fi
            warn "Rust 工具链未配置默认版本，尝试为 ${rust_user:-$USER} 安装 stable..."
            if [[ -n "$rust_user" && "$rust_user" != root ]] && command -v runuser >/dev/null 2>&1; then
                if ! runuser -u "$rust_user" -- env \
                    HOME="$rust_home" \
                    RUSTUP_HOME="${RUSTUP_HOME:-$rust_home/.rustup}" \
                    CARGO_HOME="${CARGO_HOME:-$rust_home/.cargo}" \
                    PATH="$PATH" rustup default stable; then
                    fatal "rustup default stable 失败，请先手动执行：
  rustup default stable"
                fi
            elif ! rustup default stable; then
                fatal "rustup default stable 失败，请先手动执行：
  rustup default stable"
            fi
        fi
    fi
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fatal "缺少命令 '$1'，请先安装构建依赖"
}

install_packages() {
    setup_rust_path
    setup_rust_toolchain
    local missing=()
    local cmd
    for cmd in git cargo rustc g++ make pkg-config curl pgrep readlink; do
        command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
    done

    # 音频输出编译依赖：上游 rodio→cpal 重构后，Linux 需要
    # ALSA / PipeWire / PulseAudio 三套头文件（见 audio-engine-core/Cargo.toml）。
    # 不能只在缺命令时才安装：命令齐全但缺系统库时同样无法编译。
    local missing_libs=()
    pkg-config --exists alsa || missing_libs+=("alsa")
    pkg-config --exists libpipewire-0.3 || missing_libs+=("libpipewire-0.3")
    pkg-config --exists libpulse || missing_libs+=("libpulse")

    if [[ ${#missing[@]} -eq 0 && ${#missing_libs[@]} -eq 0 ]]; then
        return 0
    fi
    [[ ${#missing[@]} -gt 0 ]] && warn "缺少构建命令：${missing[*]}"
    [[ ${#missing_libs[@]} -gt 0 ]] && warn "缺少音频开发库（pkg-config 检测）：${missing_libs[*]}"

    if command -v apt-get >/dev/null 2>&1; then
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            build-essential pkg-config curl git ca-certificates \
            libasound2-dev libdbus-1-dev \
            libpipewire-0.3-dev libpulse-dev
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y gcc-c++ make pkg-config curl git ca-certificates \
            alsa-lib-devel dbus-devel \
            pipewire-devel pulseaudio-libs-devel
    elif command -v pacman >/dev/null 2>&1; then
        pacman -Sy --needed --noconfirm base-devel pkgconf curl git ca-certificates \
            alsa-lib dbus pipewire libpulse
    else
        fatal "无法自动安装依赖，请手动安装：${missing[*]} ${missing_libs[*]}"
    fi

    command -v cargo >/dev/null 2>&1 || fatal "未检测到 Rust/Cargo，请安装 rustup 或系统 Rust 包"
    command -v rustc >/dev/null 2>&1 || fatal "未检测到 Rust 编译器，请安装 rustup 或系统 Rust 包"
}

resolve_run_user() {
    if [[ -n "${SPLAYER_USER:-}" ]]; then
        RUN_USER="$SPLAYER_USER"
    elif [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != root ]]; then
        RUN_USER="$SUDO_USER"
    else
        RUN_USER="splayer"
    fi

    if ! id "$RUN_USER" >/dev/null 2>&1; then
        useradd --system --home-dir "$DATA_DIR" --create-home --shell /usr/sbin/nologin "$RUN_USER"
    fi
    RUN_GROUP="$(id -gn "$RUN_USER")"
}

# 发现 DirettaHostSDK_* 目录：搜索根取 SPLAYER_SDK_BASE 或原调用用户 home
# （sudo 会把 HOME 改为 /root）。结果写入 SDK_LIST 与 SDK_SEARCH_BASE，供主流程与菜单共用。
discover_sdks() {
    local sdk_base="${SPLAYER_SDK_BASE:-}"
    if [[ -z "$sdk_base" ]]; then
        local sudo_home="/root"
        if [[ -n "${SUDO_USER:-}" ]] && command -v getent >/dev/null 2>&1; then
            sudo_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
        fi
        sdk_base="$sudo_home"
    fi
    SDK_SEARCH_BASE="$sdk_base"
    SDK_LIST=()
    local d
    for d in "$sdk_base"/DirettaHostSDK_*; do
        # 用 if 而非 `[[ ]] &&`：set -e 下后者会让函数在末路径缺失时返回 1，中断主流程
        if [[ -d "$d" ]]; then
            SDK_LIST+=("$d")
        fi
    done
}

# 解析 SDK 目录：--sdk-dir > DIRETTA_SDK_DIR（已在变量区处理）>
# --sdk-version > 自动发现原调用用户 home 下的 DirettaHostSDK_*
resolve_sdk() {
    if [[ -n "$SDK_DIR" ]]; then
        log "使用指定 Diretta SDK：$SDK_DIR"
        return 0
    fi

    # SDK 搜索根：默认取原调用用户的 home（sudo 会把 HOME 改为 /root）
    discover_sdks
    local -a sdks=("${SDK_LIST[@]}")
    if [[ ${#sdks[@]} -eq 0 ]]; then
        fatal "在 $SDK_SEARCH_BASE 下未发现 DirettaHostSDK_* 目录，请使用 --sdk-dir 或 --sdk-version 指定"
    fi

    if [[ -n "$SDK_VERSION" ]]; then
        local matched="" d
        for d in "${sdks[@]}"; do
            [[ "$(basename "$d")" == "DirettaHostSDK_${SDK_VERSION}" ]] && { matched="$d"; break; }
        done
        if [[ -z "$matched" ]]; then
            fatal "未找到版本 ${SDK_VERSION} 的 SDK，可用：$(printf '%s ' "${sdks[@]##*/}")"
        fi
        SDK_DIR="$matched"
        log "使用 Diretta SDK ${SDK_VERSION}：$SDK_DIR"
        return 0
    fi

    if [[ ${#sdks[@]} -eq 1 ]]; then
        SDK_DIR="${sdks[0]}"
        log "使用自动发现的 Diretta SDK：$SDK_DIR"
        return 0
    fi

    fatal "发现多个 Diretta SDK：$(printf '%s ' "${sdks[@]##*/}")。
请用 --sdk-version <N> 或 --sdk-dir 显式指定，避免静默切换 SDK 版本"
}

check_sdk() {
    [[ -d "$SDK_DIR" ]] || fatal "Diretta SDK 不存在：$SDK_DIR。请使用 --sdk-dir 或 --sdk-version 指定"
    [[ -d "$SDK_DIR/Host" ]] || fatal "Diretta SDK 缺少 Host 目录：$SDK_DIR/Host"
    [[ -d "$SDK_DIR/lib" ]] || fatal "Diretta SDK 缺少 lib 目录：$SDK_DIR/lib"

    case "$(uname -m)" in
        x86_64|amd64)
            local found=0
            for lib in "$SDK_DIR"/lib/libDirettaHost_x64-linux-*.a; do
                [[ -f "$lib" ]] && found=1 && break
            done
            [[ $found -eq 1 ]] || fatal "SDK 中未找到 x86_64 DirettaHost 静态库"
            ;;
        aarch64|arm64)
            [[ -f "$SDK_DIR/lib/libDirettaHost_aarch64-linux-15-nolog.a" ]] || \
                fatal "SDK 中未找到 aarch64 DirettaHost 静态库"
            ;;
        *)
            fatal "不支持的 Linux CPU 架构：$(uname -m)"
            ;;
    esac
}

cleanup_old_installations() {
    [[ $CLEAN_OLD -eq 1 ]] || { warn "按参数跳过旧版本清理"; return 0; }

    log "清理旧版本 SPlayer Headless 服务和运行进程"
    local service
    for service in "${OLD_SERVICE_NAMES[@]}"; do
        if systemctl list-unit-files "$service" --no-legend 2>/dev/null | grep -q "$service"; then
            systemctl disable --now "$service" >/dev/null 2>&1 || true
        fi
        rm -f "/etc/systemd/system/$service" "/lib/systemd/system/$service" "/usr/lib/systemd/system/$service"
    done

    # 只匹配已知安装路径/可执行文件名，避免误杀其他用户程序。
    # 同时清理测试中直接运行的旧版本（非服务启动的进程）。
    local pid exe
    while read -r pid; do
        [[ -n "$pid" ]] || continue
        exe="$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)"
        case "$exe" in
            /opt/splayer-headless/*|/usr/local/bin/splayer-headless|/usr/bin/splayer-headless|*/splayer-headless|$PROJECT_ROOT/target/release/headless-server|$PROJECT_ROOT/target/debug/headless-server)
                log "停止旧版本进程 PID=$pid ($exe)"
                kill "$pid" 2>/dev/null || true
                ;;
        esac
    done < <(pgrep -f '(^|/)(headless-server|splayer-headless)([[:space:]]|$)' || true)

    # 等待进程完全退出
    for _ in {1..5}; do
        local remaining=0
        while read -r _pid; do
            [[ -n "$_pid" ]] && remaining=$((remaining + 1)) && break
        done < <(pgrep -f '(^|/)(headless-server|splayer-headless)([[:space:]]|$)' || true)
        [[ $remaining -eq 0 ]] && break
        sleep 1
    done

    systemctl daemon-reload
}

build_server() {
    if [[ $SKIP_BUILD -eq 1 ]]; then
        # 跳过编译必须已有产物，否则安装无法继续
        [[ -x "$PROJECT_ROOT/target/release/headless-server" ]] || \
            fatal "--skip-build 但未找到编译产物：$PROJECT_ROOT/target/release/headless-server
请去掉 --skip-build 执行一次完整编译，或先运行 cargo build --release --package headless-server"
        log "跳过编译，使用现有产物：$PROJECT_ROOT/target/release/headless-server"
        return 0
    fi
    log "开始编译 headless-server"
    cd "$PROJECT_ROOT"
    export DIRETTA_SDK_DIR="$SDK_DIR"
    export DIRETTA_ARCH="${DIRETTA_ARCH:-auto}"
    cargo build --release --package headless-server
    [[ -x "$PROJECT_ROOT/target/release/headless-server" ]] || fatal "headless-server 编译产物不存在"
}

build_web() {
    [[ $SKIP_WEB -eq 1 ]] && { warn "按参数跳过 Web UI 构建"; return 0; }
    # 跳过编译时复用现有 Web 产物；产物缺失则仍执行构建
    if [[ $SKIP_BUILD -eq 1 && -f "$PROJECT_ROOT/out/renderer/index.html" ]]; then
        log "跳过 Web 构建，使用现有产物：$PROJECT_ROOT/out/renderer"
        return 0
    fi
    log "开始编译 Web UI"
    cd "$PROJECT_ROOT"

    # 尽量以原调用用户身份构建：root 构建会让 out/、node_modules/.vite
    # 产物归 root，普通用户后续本地构建/开发会 EACCES
    local web_user="${SUDO_USER:-}"
    if [[ -z "$web_user" || "$web_user" == root ]]; then
        # root 直接登录部署（无原用户可回退），按 root 构建
        require_cmd pnpm
        pnpm exec electron-vite build
    else
        build_web_as_user "$web_user"
    fi

    # electron-vite 会生成 out/renderer；这里不构建 Electron 安装包。
    [[ -f "$PROJECT_ROOT/out/renderer/index.html" ]] || \
        fatal "Web UI 构建完成但未找到 out/renderer/index.html"
}

# sudo 场景下以原调用用户身份运行 Web 构建：
# 1) 产物归原用户所有，不破坏其本地开发环境
# 2) 使用用户自己的 node/pnpm 工具链（runuser 非交互 shell 不加载 nvm，
#    需在常见安装位置显式探测 pnpm）
build_web_as_user() {
    local web_user="$1"
    local user_home
    user_home="$(getent passwd "$web_user" | cut -d: -f6)"

    local candidate
    local -a pnpm_candidates=()
    for candidate in \
        "$user_home/.local/share/pnpm/pnpm" \
        "$user_home"/.nvm/versions/node/*/bin/pnpm \
        "$user_home/.volta/bin/pnpm" \
        /usr/local/bin/pnpm \
        /usr/bin/pnpm; do
        [[ -x "$candidate" ]] && pnpm_candidates+=("$candidate")
    done
    if [[ ${#pnpm_candidates[@]} -eq 0 ]]; then
        fatal "未找到用户 ${web_user} 的 pnpm，请先以该用户安装 pnpm，或使用 --skip-web 跳过 Web 构建"
    fi
    # 多个版本并存（如 nvm 多 node 版本）时取版本号最大的
    local pnpm_bin
    pnpm_bin="$(printf '%s\n' "${pnpm_candidates[@]}" | sort -V | tail -n 1)"

    # 修复此前 root 构建遗留的所有权，避免用户侧构建 EACCES
    chown -R "$web_user" "$PROJECT_ROOT/out" 2>/dev/null || true
    if [[ -d "$PROJECT_ROOT/node_modules/.vite" ]]; then
        chown -R "$web_user" "$PROJECT_ROOT/node_modules/.vite" 2>/dev/null || true
    fi

    log "以用户 ${web_user} 构建 Web UI（pnpm: $pnpm_bin）"
    if ! runuser -u "$web_user" -- env \
        HOME="$user_home" \
        PATH="$(dirname "$pnpm_bin"):/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
        pnpm exec electron-vite build; then
        fatal "Web UI 构建失败，请检查以上输出"
    fi
}

install_files() {
    log "安装文件到 $INSTALL_DIR"
    install -d -m 0755 "$INSTALL_DIR" "$CONFIG_DIR" "$DATA_DIR"
    install -m 0755 "$PROJECT_ROOT/target/release/headless-server" "$BIN_PATH"

    if [[ $SKIP_WEB -eq 0 ]]; then
        # 先清理旧版前端文件再重装，避免新旧版本文件混杂
        rm -rf "$WEB_DIR"
        install -d -m 0755 "$WEB_DIR"
        cp -a "$PROJECT_ROOT/out/renderer/." "$WEB_DIR/"
        log "已清理旧版前端文件并重新安装：$WEB_DIR"
    elif [[ -d "$WEB_DIR" ]]; then
        warn "按参数跳过 Web UI 构建与前端更新，保留现有前端文件：$WEB_DIR"
    fi

    chown -R "$RUN_USER:$RUN_GROUP" "$DATA_DIR"
    chown -R root:root "$INSTALL_DIR" "$CONFIG_DIR"
    chmod 0755 "$BIN_PATH"
}

install_config() {
    if [[ ! -f "$CONFIG_PATH" ]]; then
        if [[ -f "/etc/splayer-headless/config.yaml" ]]; then
            log "迁移现有配置：/etc/splayer-headless/config.yaml -> $CONFIG_PATH"
            install -d -m 0755 "$CONFIG_DIR"
            cp -a "/etc/splayer-headless/config.yaml" "$CONFIG_PATH"
            chmod 0644 "$CONFIG_PATH"
        else
            cat > "$CONFIG_PATH" <<EOF
# SPlayer Linux Headless 配置
listen_addr: "0.0.0.0:${PORT}"
cors_origins: "*"
api_token: null
cover_cache_dir: "${DATA_DIR}/covers"
database_path: "${DATA_DIR}/library.db"
web_root: "${WEB_DIR}"
# Diretta Target 地址可写为：fe80::xxxx%2 或 IP%ifno,port
diretta_target: null
EOF
            chmod 0644 "$CONFIG_PATH"
        fi
    else
        warn "保留现有配置：$CONFIG_PATH"
    fi
}

install_service() {
    cat > "$SERVICE_PATH" <<EOF
[Unit]
Description=SPlayer Linux Headless Music Server
After=network-online.target sound.target
Wants=network-online.target

[Service]
Type=simple
User=${RUN_USER}
Group=${RUN_GROUP}
WorkingDirectory=${INSTALL_DIR}
Environment=RUST_LOG=headless_server=info,audio_engine_core=info
Environment=SPLAYER_DATA_DIR=${DATA_DIR}
Environment=SPLAYER_CONFIG_PATH=${CONFIG_PATH}
ExecStart=${BIN_PATH}
Restart=on-failure
RestartSec=3
LimitRTPRIO=infinity
LimitMEMLOCK=infinity
AmbientCapabilities=CAP_SYS_NICE CAP_NET_RAW CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_SYS_NICE CAP_NET_RAW CAP_NET_BIND_SERVICE
PrivateTmp=true
ProtectSystem=full
ReadWritePaths=${DATA_DIR}

[Install]
WantedBy=multi-user.target
EOF

    systemctl daemon-reload
    systemctl enable "$SERVICE_NAME"
    if [[ $NO_START -eq 0 ]]; then
        systemctl restart "$SERVICE_NAME"
    fi
}

smoke_test() {
    [[ $NO_START -eq 1 ]] && return 0
    log "执行 HTTP 健康检查"
    local url="http://127.0.0.1:${PORT}/api/v1/diretta/status"
    for _ in {1..20}; do
        if curl --fail --silent --show-error "$url" >/dev/null 2>&1; then
            log "服务已启动，健康检查通过：$url"
            return 0
        fi
        sleep 1
    done
    systemctl --no-pager --full status "$SERVICE_NAME" || true
    fatal "服务已启动但 HTTP 健康检查失败，请查看：journalctl -u $SERVICE_NAME"
}

# 探测曲库数据库文件：标准布局在 DATA_DIR，旧版部署可能放在程序目录内
detect_db_files() {
    DB_FILES=()
    local db
    for db in "$DATA_DIR/library.db" "$INSTALL_DIR/data/library.db" "$INSTALL_DIR/library.db"; do
        # 用 if 而非 `[[ ]] &&`：set -e 下后者会让函数在末路径缺失时返回 1，中断主流程
        if [[ -f "$db" ]]; then
            DB_FILES+=("$db")
        fi
    done
}

# 危险操作确认：--yes 直接放行；非交互终端（无 TTY）且未带 --yes 时拒绝，
# 避免管道/自动化场景误删数据；交互终端要求输入 yes 确认
confirm_or_abort() {
    local msg="$1"
    if [[ $ASSUME_YES -eq 1 ]]; then
        warn "--yes 已指定，跳过交互确认：$msg"
        return 0
    fi
    if [[ ! -t 0 ]]; then
        fatal "非交互环境无法确认：$msg；确认无误请附加 --yes 重新执行"
    fi
    local reply=""
    read -r -p "输入 yes 确认执行，其他任意内容取消: " reply || reply=""
    [[ "$reply" == "yes" ]] || fatal "已取消：$msg"
}

# 卸载已部署的服务与程序。默认保留配置与数据，--purge 连同删除。
uninstall_all() {
    require_cmd systemctl

    # 明确告知数据库去留，避免误删曲库；确认必须先于一切删除动作，取消即无副作用
    detect_db_files
    if [[ $PURGE -eq 1 ]]; then
        if [[ ${#DB_FILES[@]} -gt 0 ]]; then
            warn "将永久删除数据库文件：${DB_FILES[*]}"
        fi
        confirm_or_abort "--purge 将永久删除程序、配置与数据"
    else
        [[ ${#DB_FILES[@]} -gt 0 ]] && log "数据库文件将保留：${DB_FILES[*]}"
    fi

    # 卸载必须清理旧服务/进程，忽略 --no-clean
    CLEAN_OLD=1 cleanup_old_installations

    log "移除程序目录：$INSTALL_DIR"
    if [[ $PURGE -eq 1 ]]; then
        rm -rf "$INSTALL_DIR"
    else
        # 默认只删程序文件；目录内可能还有数据/配置等非程序文件
        # （如旧版部署的 data/、config.yaml），一并保留
        rm -f "$BIN_PATH"
        rm -rf "$WEB_DIR"
        if [[ -n "$(find "$INSTALL_DIR" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]]; then
            warn "$INSTALL_DIR 内仍有非程序文件（数据/配置等），已保留；彻底清空请加 --purge"
        fi
    fi
    rm -f "$SERVICE_PATH"
    systemctl daemon-reload

    if [[ $PURGE -eq 1 ]]; then
        log "删除配置与数据：$CONFIG_DIR $DATA_DIR"
        rm -rf "$CONFIG_DIR" "$DATA_DIR"
    else
        warn "已保留配置与数据（$CONFIG_DIR $DATA_DIR），彻底删除请加 --purge"
    fi

    log "卸载完成"
    log "如不再需要可手动删除服务用户：userdel <用户名>"
}

# 无参数且在交互终端运行时显示菜单选择功能；
# 无参数且非交互（管道/CI）保持默认安装；带参数为脚本化直接执行，互不影响。
# 注意：须位于所有函数定义之后（菜单会调用 choose_sdk_in_menu → discover_sdks）
if [[ $ARG_COUNT -eq 0 && -t 0 ]]; then
    show_menu
fi

[[ $EUID -eq 0 ]] || fatal "请使用 sudo 运行此脚本"
[[ "$(uname -s)" == "Linux" ]] || fatal "此脚本只支持 Linux"

# --purge 仅在卸载时有意义
if [[ $PURGE -eq 1 && $UNINSTALL -eq 0 ]]; then
    warn "--purge 仅与 --uninstall 搭配使用，已忽略"
    PURGE=0
fi

# 卸载分支：只停服务、删程序，不编译、不依赖 SDK
if [[ $UNINSTALL -eq 1 ]]; then
    uninstall_all
    exit 0
fi

# 就地更新提醒：默认安装与 --update 行为一致（清理旧程序与前端文件后重装、保留配置与数据库）
detect_existing_install
if [[ ${#EXISTING_PARTS[@]} -gt 0 ]]; then
    detect_db_files
    if [[ ${#DB_FILES[@]} -gt 0 ]]; then
        log "检测到现有部署，本次为就地更新；配置与数据库将保留：$CONFIG_PATH ${DB_FILES[*]}"
    else
        log "检测到现有部署，本次为就地更新；现有配置与数据将保留"
    fi
elif [[ $UPDATE -eq 1 ]]; then
    warn "尚未部署过服务，本次将执行全新安装"
fi

install_packages
require_cmd systemctl
require_cmd install
resolve_run_user
if [[ $SKIP_BUILD -eq 1 ]]; then
    # 不编译则不链接 SDK，无需解析与校验
    log "按参数跳过编译与 SDK 检测"
else
    resolve_sdk
    check_sdk
fi
cleanup_old_installations
build_server
build_web
install_files
install_config
install_service
smoke_test

log "部署完成"
log "访问地址：http://$(hostname -I 2>/dev/null | awk '{print $1}'):${PORT}/"
log "配置文件：$CONFIG_PATH"
log "数据目录：$DATA_DIR"
log "查看日志：journalctl -u $SERVICE_NAME -f"
log "停止服务：systemctl stop $SERVICE_NAME"
