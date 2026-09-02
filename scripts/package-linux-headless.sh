#!/usr/bin/env bash
set -Eeuo pipefail
#
# SPlayer Linux Headless 独立免编译发布包一键打包脚本（支持智能增量编译与多架构秒级复用）
#
# 特性：
#   1. 支持智能增量编译：前端 Web UI 与 Rust 服务端在源码未变动时自动跳过，秒级完成打包；
#   2. 多架构产物缓存隔离：各 CPU 变体独立缓存 (v2, v3, v4, zen4)，重复打包无需重编；
#   3. 支持交互式菜单与命令行参数指定；
#   4. 自动校验动态链接库依赖 (ldd)，确保无缺失依赖；
#   5. 支持 --force 强制全量重新编译。
#

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_BASE_DIR="/home/songlian"
SDK_DIR="${DIRETTA_SDK_DIR:-}"
SDK_VERSION="150"
TARGET_CPU_ARCH=""
BUILD_DATE="$(date +%Y%m%d)"
SKIP_BUILD=0
SKIP_WEB=0
CREATE_TAR=1
FORCE_REBUILD=0

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

log()     { echo -e "${GREEN}[package]${NC} $*"; }
info()    { echo -e "${BLUE}[package]${NC} $*"; }
warn()    { echo -e "${YELLOW}[package][WARN]${NC} $*"; }
error()   { echo -e "${RED}[package][ERROR]${NC} $*" >&2; }
fatal()   { error "$*"; exit 1; }

usage() {
    echo "用法: $0 [选项]"
    echo
    echo "选项:"
    echo "  --arch <variant>       CPU 微架构: v2, v3, v4, zen4, auto, all (默认交互选择或 v2)"
    echo "  --sdk-dir <path>       指定 DirettaHostSDK 路径"
    echo "  --sdk-version <ver>    指定 Diretta SDK 版本（如 150/149/148）"
    echo "  --output-dir <path>    发布包输出根目录（默认 /home/songlian）"
    echo "  --date <YYYYMMDD>      指定打包日期标识（默认今天：${BUILD_DATE}）"
    echo "  --force, -f            强制全量重新构建（跳过前端与 Rust 增量缓存）"
    echo "  --skip-build           跳过 Rust 编译，直接使用现有构建产物"
    echo "  --skip-web             跳过前端 Web UI 构建"
    echo "  --no-tar               仅生成发布目录，不生成 .tar.gz 压缩包"
    echo "  --help, -h             显示本帮助信息"
}

# 发现可用 Diretta SDK（去重并按版本号倒序排列，最新版本排在首位）
discover_sdks() {
    SDK_LIST=()
    local -A seen=()
    local raw_list=()
    for base in "$HOME" "/opt"; do
        for d in "$base"/DirettaHostSDK_*; do
            if [[ -d "$d" ]]; then
                local real_d
                real_d="$(readlink -f "$d" 2>/dev/null || echo "$d")"
                if [[ -z "${seen[$real_d]:-}" ]]; then
                    seen["$real_d"]=1
                    raw_list+=("$real_d")
                fi
            fi
        done
    done

    # 按照版本号倒序排列（如 150 -> 149 -> 148）
    if [[ ${#raw_list[@]} -gt 0 ]]; then
        while IFS= read -r item; do
            [[ -n "$item" ]] && SDK_LIST+=("$item")
        done < <(printf '%s\n' "${raw_list[@]}" | sort -V -r)
    fi
}

# 交互式菜单：选择 SDK
menu_select_sdk() {
    discover_sdks
    if [[ -n "$SDK_DIR" ]]; then
        info "已指定 SDK: ${SDK_DIR}"
        return 0
    fi

    if [[ ${#SDK_LIST[@]} -eq 0 ]]; then
        warn "未发现已安装的 DirettaHostSDK_* 目录，将使用默认版本: 150"
        return 0
    fi

    echo -e "${CYAN}======================================================${NC}"
    echo -e "${BOLD}  第 1 步：选择 Diretta SDK 版本${NC}"
    echo -e "${CYAN}======================================================${NC}"
    local i=1
    for d in "${SDK_LIST[@]}"; do
        echo -e "  ${BOLD}${i})${NC} $(basename "$d") (${d})"
        i=$((i + 1))
    done
    echo -e "  ${BOLD}0)${NC} 默认 / 手动指定"
    echo

    while true; do
        read -r -p "请选择 Diretta SDK [1-${#SDK_LIST[@]}, 默认 1]: " choice || fatal "读取选择失败"
        choice="${choice:-1}"
        if [[ "$choice" == "0" ]]; then
            break
        elif [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= ${#SDK_LIST[@]} )); then
            SDK_DIR="${SDK_LIST[$((choice - 1))]}"
            local bname
            bname="$(basename "$SDK_DIR")"
            if [[ "$bname" =~ _([0-9]+)$ ]]; then
                SDK_VERSION="${BASH_REMATCH[1]}"
            fi
            log "已选择 SDK: ${SDK_DIR} (版本: ${SDK_VERSION})"
            break
        fi
        warn "无效输入: $choice"
    done
}

# 交互式菜单：选择 CPU 变体
menu_select_arch() {
    if [[ -n "$TARGET_CPU_ARCH" ]]; then
        return 0
    fi

    echo
    echo -e "${CYAN}======================================================${NC}"
    echo -e "${BOLD}  第 2 步：选择 CPU 微架构变体${NC}"
    echo -e "${CYAN}======================================================${NC}"
    echo -e "  ${BOLD}1) v2 (x86-64-v2)${NC}   🌟 【推荐】通用兼容版（SSE4.2/POPCNT，支持 J4125/N5105/N100/虚拟机/旧款CPU）"
    echo -e "  ${BOLD}2) v3 (x86-64-v3)${NC}   ⚡ 主流高性能版（AVX2/FMA/BMI2，酷睿8代+/Zen1~3，若目标CPU不支持会报错）"
    echo -e "  ${BOLD}3) v4 (x86-64-v4)${NC}   🚀 极限服务器版（AVX-512，酷睿11代+/Xeon）"
    echo -e "  ${BOLD}4) zen4 (znver4)${NC}    AMD Zen4 专属深度优化（Ryzen 7000+/EPYC Genoa）"
    echo -e "  ${BOLD}5) auto${NC}             自动探测当前编译主机的 CPU 指令集"
    echo -e "  ${BOLD}6) all${NC}              一键批量编译并打包所有架构变体 (v2, v3, v4, zen4)"
    echo

    while true; do
        read -r -p "请选择 CPU 变体 [1-6, 默认 1 (v2 通用兼容)]: " choice || fatal "读取选择失败"
        choice="${choice:-1}"
        case "$choice" in
            1) TARGET_CPU_ARCH="v2"; break ;;
            2) TARGET_CPU_ARCH="v3"; break ;;
            3) TARGET_CPU_ARCH="v4"; break ;;
            4) TARGET_CPU_ARCH="zen4"; break ;;
            5) TARGET_CPU_ARCH="auto"; break ;;
            6) TARGET_CPU_ARCH="all"; break ;;
            v2|v3|v4|zen4|auto|all) TARGET_CPU_ARCH="$choice"; break ;;
            *) warn "无效输入: $choice" ;;
        esac
    done
    log "已选择 CPU 架构变体: ${TARGET_CPU_ARCH}"
}

# 解析命令行参数
ARG_COUNT=$#
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)
            [[ $# -ge 2 ]] || fatal "--arch 需要参数 (v2, v3, v4, zen4, auto, all)"
            TARGET_CPU_ARCH="$2"
            shift 2
            ;;
        --sdk-dir)
            [[ $# -ge 2 ]] || fatal "--sdk-dir 需要参数"
            SDK_DIR="$2"
            shift 2
            ;;
        --sdk-version)
            [[ $# -ge 2 ]] || fatal "--sdk-version 需要参数"
            SDK_VERSION="$2"
            shift 2
            ;;
        --output-dir)
            [[ $# -ge 2 ]] || fatal "--output-dir 需要参数"
            OUTPUT_BASE_DIR="$2"
            shift 2
            ;;
        --date)
            [[ $# -ge 2 ]] || fatal "--date 需要参数 (如 20260831)"
            BUILD_DATE="$2"
            shift 2
            ;;
        --force|-f)
            FORCE_REBUILD=1
            shift
            ;;
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        --skip-web)
            SKIP_WEB=1
            shift
            ;;
        --no-tar)
            CREATE_TAR=0
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

# 如果无参数且在交互终端，弹出菜单
if [[ $ARG_COUNT -eq 0 && -t 0 ]]; then
    menu_select_sdk
    menu_select_arch
fi

# 默认架构为 v2 通用兼容
TARGET_CPU_ARCH="${TARGET_CPU_ARCH:-v2}"

# 自动推导 SDK 目录
if [[ -z "$SDK_DIR" ]]; then
    for candidate in \
        "/home/songlian/DirettaHostSDK_${SDK_VERSION}" \
        "$HOME/DirettaHostSDK_${SDK_VERSION}" \
        "/opt/DirettaHostSDK_${SDK_VERSION}" \
        /home/songlian/DirettaHostSDK_* \
        "$HOME"/DirettaHostSDK_*; do
        if [[ -d "$candidate" ]]; then
            SDK_DIR="$candidate"
            break
        fi
    done
fi

ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64) ARCH_NAME="x86_64" ;;
    aarch64|arm64) ARCH_NAME="aarch64" ;;
    *) ARCH_NAME="$ARCH" ;;
esac

# 检查前端 Web UI 是否需要重新构建
check_web_needs_build() {
    if [[ $FORCE_REBUILD -eq 1 ]]; then
        return 0
    fi
    if [[ ! -f "$WEB_DIST/index.html" ]]; then
        return 0
    fi
    # 查找是否有比现有构建产物更新的前端源码文件
    local newer_src
    newer_src=$(find "$PROJECT_ROOT/src" "$PROJECT_ROOT/shared" "$PROJECT_ROOT/package.json" "$PROJECT_ROOT/electron.vite.config.ts" "$PROJECT_ROOT/index.html" -type f -newer "$WEB_DIST/index.html" 2>/dev/null | head -n 1 || true)
    if [[ -n "$newer_src" ]]; then
        return 0
    fi
    return 1
}

# 检查指定架构的 Rust 产物是否需要重新编译
check_rust_needs_build() {
    local arch_var="$1"
    local cached_bin="$PROJECT_ROOT/target/release/headless-server-${arch_var}"
    local stamp_file="$PROJECT_ROOT/target/release/.stamp-${arch_var}"

    if [[ $FORCE_REBUILD -eq 1 ]]; then
        return 0
    fi
    if [[ ! -x "$cached_bin" || ! -f "$stamp_file" ]]; then
        return 0
    fi

    # 检查 SDK 路径与版本是否改变
    local current_sdk_sig="${SDK_DIR:-default}_${SDK_VERSION}"
    local cached_sdk_sig
    cached_sdk_sig=$(cat "$stamp_file" 2>/dev/null || echo "")
    if [[ "$current_sdk_sig" != "$cached_sdk_sig" ]]; then
        return 0
    fi

    # 检查 native/ 目录、Cargo.toml、Cargo.lock 是否有更新
    local newer_rust_file
    newer_rust_file=$(find "$PROJECT_ROOT/native" "$PROJECT_ROOT/Cargo.toml" "$PROJECT_ROOT/Cargo.lock" -type f -newer "$cached_bin" 2>/dev/null | head -n 1 || true)
    if [[ -n "$newer_rust_file" ]]; then
        return 0
    fi

    return 1
}

# 单架构打包函数
package_single_arch() {
    local arch_var="$1"
    local rust_target_cpu=""
    local arch_desc=""

    case "$arch_var" in
        v2)
            rust_target_cpu="x86-64-v2"
            arch_desc="x86-64-v2 通用兼容版（广泛支持 J4125/N5105/N100/虚拟机等无 AVX2 设备）"
            ;;
        v3)
            rust_target_cpu="x86-64-v3"
            arch_desc="x86-64-v3 高性能版（需 AVX2/FMA 支持，酷睿8代+/Zen1~3）"
            ;;
        v4)
            rust_target_cpu="x86-64-v4"
            arch_desc="x86-64-v4 极限服务器版（需 AVX-512 支持，酷睿11代+/Xeon）"
            ;;
        zen4)
            rust_target_cpu="znver4"
            arch_desc="AMD Zen4 专属深度优化版（Ryzen 7000+/EPYC Genoa）"
            ;;
        auto)
            rust_target_cpu="native"
            arch_desc="当前编译机原生硬件优化版 (native)"
            ;;
        *)
            rust_target_cpu="x86-64-v2"
            arch_desc="${arch_var}"
            ;;
    esac

    local pkg_name="splayer-headless-linux-${ARCH_NAME}-${arch_var}-diretta-sdk${SDK_VERSION}-${BUILD_DATE}"
    local pkg_dir="${OUTPUT_BASE_DIR}/${pkg_name}"
    local tar_file="${OUTPUT_BASE_DIR}/${pkg_name}.tar.gz"
    local cached_bin="$PROJECT_ROOT/target/release/headless-server-${arch_var}"
    local stamp_file="$PROJECT_ROOT/target/release/.stamp-${arch_var}"

    echo
    echo -e "${CYAN}======================================================${NC}"
    log "正在打包架构变体 : ${BOLD}${arch_var}${NC} (${arch_desc})"
    log "Diretta SDK      : ${SDK_DIR:-默认}"
    log "打包目录         : ${pkg_dir}"
    log "压缩包路径       : ${tar_file}"
    echo -e "${CYAN}======================================================${NC}"

    # 1. 增量编译 Rust 服务端
    if [[ $SKIP_BUILD -eq 0 ]]; then
        if check_rust_needs_build "$arch_var"; then
            log "增量编译 Rust release 产物 (CPU: ${arch_var}, target-cpu: ${rust_target_cpu})..."
            cd "$PROJECT_ROOT"
            if [[ -n "$SDK_DIR" ]]; then
                export DIRETTA_SDK_DIR="$SDK_DIR"
            fi
            export DIRETTA_ARCH="${arch_var}"
            export CARGO_INCREMENTAL=1

            cargo build --release --package headless-server

            mkdir -p "$PROJECT_ROOT/target/release"
            cp -f "$PROJECT_ROOT/target/release/headless-server" "$cached_bin"
            echo "${SDK_DIR:-default}_${SDK_VERSION}" > "$stamp_file"
            log "架构 ${arch_var} 编译成功并已缓存产物"
        else
            log "⚡ 检测到架构 ${arch_var} 现有编译产物已是最新，跳过重新编译 (耗时 0s)"
        fi
    else
        log "跳过 Rust 编译，使用现有产物"
    fi

    local server_bin="$cached_bin"
    if [[ ! -x "$server_bin" ]]; then
        server_bin="$PROJECT_ROOT/target/release/headless-server"
    fi
    [[ -x "$server_bin" ]] || fatal "未找到编译产物：$server_bin"

    # 2. 检查动态链接库依赖
    log "检查二进制文件动态链接库依赖 (ldd)..."
    if command -v ldd >/dev/null 2>&1; then
        if ldd "$server_bin" | grep -q "not found"; then
            warn "警告：检测到缺失的动态库依赖："
            ldd "$server_bin" | grep "not found"
            fatal "打包终止：二进制存在未解析的动态库依赖"
        fi
        log "动态库依赖检查通过（无 missing/not found 依赖）"
    fi

    # 3. 组装发布目录
    log "组装发布目录: ${pkg_dir}..."
    rm -rf "$pkg_dir"
    mkdir -p "$pkg_dir/config"

    install -m 0755 "$server_bin" "$pkg_dir/splayer-headless"
    cp -r "$WEB_DIST" "$pkg_dir/web"

    # 配置文件样例
    cat > "$pkg_dir/config/config.example.yaml" <<'EOF'
# SPlayer Linux Headless 配置文件
listen_addr: "0.0.0.0:14558"
cors_origins: "*"
api_token: null
cover_cache_dir: "/var/lib/splayer-headless/covers"
database_path: "/var/lib/splayer-headless/library.db"
web_root: "/opt/splayer-headless/web"
# Diretta Target 地址可写为：fe80::xxxx%2 或 IP%ifno,port
diretta_target: null
EOF

    # systemd 服务模板
    cat > "$pkg_dir/splayer-headless.service" <<'EOF'
[Unit]
Description=SPlayer Linux Headless Music Server
After=network-online.target sound.target
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
WorkingDirectory=/opt/splayer-headless
Environment=RUST_LOG=headless_server=info,audio_engine_core=info
Environment=SPLAYER_DATA_DIR=/var/lib/splayer-headless
Environment=SPLAYER_CONFIG_PATH=/opt/splayer-headless/config/config.yaml
ExecStart=/opt/splayer-headless/splayer-headless
Restart=on-failure
RestartSec=3
LimitRTPRIO=infinity
LimitMEMLOCK=infinity
AmbientCapabilities=CAP_SYS_NICE CAP_NET_RAW CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_SYS_NICE CAP_NET_RAW CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
EOF

    # 一键免编译部署脚本
    cat > "$pkg_dir/install.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# SPlayer Linux Headless 一键免编译部署脚本（独立发布包专用）
# ==============================================================================

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()   { echo -e "${GREEN}[splayer]${NC} $*"; }
warn()  { echo -e "${YELLOW}[splayer][WARN]${NC} $*"; }
error() { echo -e "${RED}[splayer][ERROR]${NC} $*" >&2; }
fatal() { error "$*"; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="/opt/splayer-headless"
CONFIG_DIR="${INSTALL_DIR}/config"
CONFIG_PATH="${CONFIG_DIR}/config.yaml"
DATA_DIR="/var/lib/splayer-headless"
SERVICE_FILE="/etc/systemd/system/splayer-headless.service"

[[ $EUID -eq 0 ]] || fatal "请使用 root 权限运行本脚本：sudo $0"

log "正在部署 SPlayer Linux Headless..."

# 1. 停止旧服务
if systemctl is-active --quiet splayer-headless.service 2>/dev/null; then
    log "停止运行中的旧服务..."
    systemctl stop splayer-headless.service || true
fi

# 2. 准备目录
mkdir -p "$INSTALL_DIR" "$CONFIG_DIR" "$DATA_DIR"

# 3. 安装程序与前端网页
log "安装主程序与 Web UI 到 ${INSTALL_DIR}..."
install -m 0755 "${SCRIPT_DIR}/splayer-headless" "${INSTALL_DIR}/splayer-headless"
rm -rf "${INSTALL_DIR}/web"
cp -r "${SCRIPT_DIR}/web" "${INSTALL_DIR}/web"

# 4. 初始化配置（不覆盖现有配置）
if [[ ! -f "$CONFIG_PATH" ]]; then
    if [[ -f "${SCRIPT_DIR}/config/config.example.yaml" ]]; then
        cp "${SCRIPT_DIR}/config/config.example.yaml" "$CONFIG_PATH"
        log "已初始化配置文件：${CONFIG_PATH}"
    fi
else
    log "保留现有配置文件：${CONFIG_PATH}"
fi

# 5. 安装 systemd 服务
log "配置 systemd 系统服务..."
install -m 0644 "${SCRIPT_DIR}/splayer-headless.service" "$SERVICE_FILE"
systemctl daemon-reload
systemctl enable splayer-headless.service
systemctl restart splayer-headless.service

sleep 2

# 6. 获取 IP 与端口
PORT=14558
if [[ -f "$CONFIG_PATH" ]]; then
    P=$(grep -E '^\s*listen_addr:' "$CONFIG_PATH" | sed -E 's/.*:([0-9]+).*/\1/' || true)
    [[ -z "$P" ]] && P=$(grep -E '^\s*port:\s*[0-9]+' "$CONFIG_PATH" | awk '{print $2}' || true)
    [[ -n "$P" ]] && PORT="$P"
fi

HOST_IP=$(hostname -I 2>/dev/null | awk '{print $1}' || echo "127.0.0.1")

echo
echo -e "${CYAN}======================================================${NC}"
echo -e "${GREEN}  SPlayer Linux Headless 部署成功！${NC}"
echo -e "${CYAN}======================================================${NC}"
echo -e "  Web 访问地址 : ${YELLOW}http://${HOST_IP}:${PORT}/${NC}"
echo -e "  配置文件路径 : ${CYAN}${CONFIG_PATH}${NC}"
echo -e "  数据存储目录 : ${CYAN}${DATA_DIR}${NC}"
echo -e "  服务运行状态 : systemctl status splayer-headless.service"
echo -e "  实时运行日志 : journalctl -u splayer-headless.service -f"
echo -e "  停止服务命令 : sudo systemctl stop splayer-headless.service"
echo -e "${CYAN}======================================================${NC}"
echo
EOF
    chmod +x "$pkg_dir/install.sh"

    # 说明文档
    cat > "$pkg_dir/README.md" <<EOF
# SPlayer Linux Headless 独立免编译发布包

## 📦 版本与构建信息

- **软件版本**：SPlayer Next Headless (Pure Rust Edition)
- **CPU 架构与微架构变体**：**${ARCH_NAME} (${arch_var})**
  - **硬件要求/描述**：${arch_desc}
- **Diretta SDK 版本**：**DirettaHostSDK v${SDK_VERSION}**
- **构建/打包日期**：**${BUILD_DATE}**
- **适用操作系统**：Linux 64 位系统（Ubuntu 20.04+、Debian 11+、Rocky Linux / CentOS 8+、Arch Linux 等）
- **运行环境依赖**：零外部额外依赖（纯原生 ALSA / Diretta，免 PipeWire），开箱即用。

---

## 🚀 快速安装部署（一键脚本）

### 1. 解压发布包
\`\`\`bash
tar -zxvf ${pkg_name}.tar.gz
cd ${pkg_name}
\`\`\`

### 2. 一键安装
\`\`\`bash
sudo ./install.sh
\`\`\`

脚本将自动完成：
- 复制主程序与 Web 资源到 \`/opt/splayer-headless\`；
- 初始化配置文件 \`/opt/splayer-headless/config/config.yaml\`；
- 安装并启动 \`splayer-headless.service\` systemd 系统服务。

### 3. 打开 Web 控制台
在电脑或手机浏览器打开：
\`\`\`text
http://<您的Linux主机IP>:14558/
\`\`\`

---

## 🛠️ 常用管理命令

- **查看运行状态**：
  \`\`\`bash
  sudo systemctl status splayer-headless.service
  \`\`\`
- **查看实时推流日志**：
  \`\`\`bash
  sudo journalctl -u splayer-headless.service -f
  \`\`\`
- **重启服务**：
  \`\`\`bash
  sudo systemctl restart splayer-headless.service
  \`\`\`
- **停止服务**：
  \`\`\`bash
  sudo systemctl stop splayer-headless.service
  \`\`\`
EOF

    # 4. 生成 tar.gz
    if [[ $CREATE_TAR -eq 1 ]]; then
        log "生成发布包压缩文件: ${tar_file}..."
        cd "$OUTPUT_BASE_DIR"
        tar -zcf "${pkg_name}.tar.gz" "${pkg_name}"
        
        local size
        size=$(du -h "$tar_file" | awk '{print $1}')
        log "打包完成！大小: ${BOLD}${size}${NC}"
        
        if command -v sha256sum >/dev/null 2>&1; then
            local checksum
            checksum=$(sha256sum "$tar_file" | awk '{print $1}')
            log "SHA256: ${checksum}"
        fi
    fi

    log "发布目录就绪: ${pkg_dir}"
}

# 自动清理调试/测试时产生的 debug 调试符号与中间依赖缓存，释放磁盘空间
clean_debug_cache() {
    local debug_dir="$PROJECT_ROOT/target/debug"
    if [[ -d "$debug_dir" ]]; then
        local debug_size
        debug_size=$(du -sh "$debug_dir" 2>/dev/null | awk '{print $1}' || echo "")
        if [[ -n "$debug_size" && "$debug_size" != "0" ]]; then
            log "🧹 检测到调试中间产物 target/debug (${debug_size})，正在自动清理以释放磁盘空间..."
            rm -rf "$debug_dir"
            log "✨ 调试缓存清理完成！"
        fi
    fi
}

# 0. 自动检查并清理调试产生的庞大中间依赖缓存
clean_debug_cache

# 1. 增量构建前端 Web UI
WEB_DIST="$PROJECT_ROOT/out/renderer"
if [[ $SKIP_BUILD -eq 0 && $SKIP_WEB -eq 0 ]]; then
    if check_web_needs_build; then
        log "构建 Web 控制台前端静态资源..."
        cd "$PROJECT_ROOT"
        pnpm exec electron-vite build
    else
        log "⚡ 检测到前端 Web UI 产物已是最新，跳过 Vite 构建 (耗时 0s)"
    fi
fi

[[ -f "$WEB_DIST/index.html" ]] || fatal "未找到 Web 前端构建产物：$WEB_DIST/index.html"

# 2. 根据架构选项执行打包
if [[ "$TARGET_CPU_ARCH" == "all" ]]; then
    log "开始全架构批量打包模式 (v2, v3, v4, zen4)..."
    for a in v2 v3 v4 zen4; do
        package_single_arch "$a"
    done
    log "全架构批量打包完成！"
else
    package_single_arch "$TARGET_CPU_ARCH"
fi

echo
log "======================================================"
log "  🎉 所有发布包构建完成！"
log "  输出位置: ${OUTPUT_BASE_DIR}"
log "======================================================"
