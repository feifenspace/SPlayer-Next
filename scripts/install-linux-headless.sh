#!/usr/bin/env bash
set -Eeuo pipefail

# SPlayer Linux Headless 一键编译、安装、部署脚本
#
# 用法：
#   sudo ./scripts/install-linux-headless.sh
#   sudo ./scripts/install-linux-headless.sh --sdk-dir /opt/DirettaHostSDK_148
#   sudo ./scripts/install-linux-headless.sh --skip-web
#   sudo ./scripts/install-linux-headless.sh --no-start
#   sudo ./scripts/install-linux-headless.sh --no-clean
#
# 默认会停止并清理已知旧版服务和 SPlayer Headless 进程；--no-clean 可跳过。
# 可选环境变量：
#   DIRETTA_SDK_DIR  DirettaHostSDK 根目录
#   SPLAYER_USER     运行服务的 Linux 用户，默认当前调用 sudo 的用户或 splayer
#   SPLAYER_PORT     HTTP 端口，默认 14558
#   SPLAYER_DATA_DIR 数据目录，默认 /var/lib/splayer-headless

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="/opt/splayer-headless"
BIN_PATH="${INSTALL_DIR}/splayer-headless"
WEB_DIR="${INSTALL_DIR}/web"
CONFIG_DIR="/etc/splayer-headless"
CONFIG_PATH="${CONFIG_DIR}/config.yaml"
DATA_DIR="${SPLAYER_DATA_DIR:-/var/lib/splayer-headless}"
SERVICE_NAME="splayer-headless.service"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}"
PORT="${SPLAYER_PORT:-14558}"
SDK_DIR="${DIRETTA_SDK_DIR:-/home/songlian/DirettaHostSDK_148}"
SKIP_WEB=0
NO_START=0
CLEAN_OLD=1

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
    sed -n '1,22p' "$0"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sdk-dir)
            [[ $# -ge 2 ]] || fatal "--sdk-dir 需要参数"
            SDK_DIR="$2"
            shift 2
            ;;
        --skip-web)
            SKIP_WEB=1
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
    [[ ${#missing[@]} -eq 0 ]] && return 0

    warn "缺少构建命令：${missing[*]}"
    if command -v apt-get >/dev/null 2>&1; then
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            build-essential pkg-config curl git ca-certificates \
            libasound2-dev libdbus-1-dev
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y gcc-c++ make pkg-config curl git ca-certificates \
            alsa-lib-devel dbus-devel
    elif command -v pacman >/dev/null 2>&1; then
        pacman -Sy --needed --noconfirm base-devel pkgconf curl git ca-certificates \
            alsa-lib dbus
    else
        fatal "无法自动安装依赖，请手动安装：${missing[*]}"
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

check_sdk() {
    [[ -d "$SDK_DIR" ]] || fatal "Diretta SDK 不存在：$SDK_DIR。请使用 --sdk-dir 指定 SDK 根目录"
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
    log "开始编译 headless-server"
    cd "$PROJECT_ROOT"
    export DIRETTA_SDK_DIR="$SDK_DIR"
    export DIRETTA_ARCH="${DIRETTA_ARCH:-auto}"
    cargo build --release --package headless-server
    [[ -x "$PROJECT_ROOT/target/release/headless-server" ]] || fatal "headless-server 编译产物不存在"
}

build_web() {
    [[ $SKIP_WEB -eq 1 ]] && { warn "按参数跳过 Web UI 构建"; return 0; }
    require_cmd pnpm
    log "开始编译 Web UI"
    cd "$PROJECT_ROOT"
    # electron-vite 会生成 out/renderer；这里不构建 Electron 安装包。
    pnpm exec electron-vite build
    [[ -f "$PROJECT_ROOT/out/renderer/index.html" ]] || \
        fatal "Web UI 构建完成但未找到 out/renderer/index.html"
}

install_files() {
    log "安装文件到 $INSTALL_DIR"
    install -d -m 0755 "$INSTALL_DIR" "$CONFIG_DIR" "$DATA_DIR"
    install -m 0755 "$PROJECT_ROOT/target/release/headless-server" "$BIN_PATH"

    if [[ $SKIP_WEB -eq 0 ]]; then
        rm -rf "$WEB_DIR"
        install -d -m 0755 "$WEB_DIR"
        cp -a "$PROJECT_ROOT/out/renderer/." "$WEB_DIR/"
    fi

    chown -R "$RUN_USER:$RUN_GROUP" "$DATA_DIR"
    chown -R root:root "$INSTALL_DIR" "$CONFIG_DIR"
    chmod 0755 "$BIN_PATH"
}

install_config() {
    if [[ ! -f "$CONFIG_PATH" ]]; then
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
ExecStart=${BIN_PATH}
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
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

[[ $EUID -eq 0 ]] || fatal "请使用 sudo 运行此脚本"
[[ "$(uname -s)" == "Linux" ]] || fatal "此脚本只支持 Linux"

install_packages
require_cmd systemctl
require_cmd install
resolve_run_user
check_sdk
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
