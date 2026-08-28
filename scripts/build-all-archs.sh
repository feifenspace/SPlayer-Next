#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# SPlayer Headless 多 CPU 变体全架构自动构建脚本
# 参考 tinyLMS-old 设计，为不同微架构编译极致优化的独立二进制发布版本
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
DIST_DIR="${PROJECT_ROOT}/dist/releases"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

info() { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
warning() { echo -e "${YELLOW}[WARNING]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; }

mkdir -p "${DIST_DIR}"

info "======================================================="
info "  SPlayer Headless Multi-Arch Release Build Pipeline   "
info "======================================================="

# 构建架构定义：(架构名 Rust_target_cpu Diretta_arch 描述)
ARCH_LIST=(
    "x86_64-v2:x86-64-v2:v2:通用基础版 (SSE4.2/POPCNT/SSSE3 广泛兼容)"
    "x86_64-v3:x86-64-v3:v3:主流高性能版 (AVX2/FMA/BMI2 酷睿8代+/Zen1~3)"
    "x86_64-v4:x86-64-v4:v4:至尊服务器版 (AVX-512 酷睿11代+/Xeon)"
    "zen4:znver4:zen4:AMD Zen4 专属深度优化 (Ryzen 7000+/EPYC Genoa)"
)

cd "${PROJECT_ROOT}"

for item in "${ARCH_LIST[@]}"; do
    IFS=':' read -r arch_name target_cpu diretta_arch desc <<< "${item}"

    info "-------------------------------------------------------"
    info "正在编译架构: ${CYAN}${arch_name}${NC}"
    info "描述: ${desc}"
    info "配置: RUSTFLAGS=\"-C target-cpu=${target_cpu}\", DIRETTA_ARCH=\"${diretta_arch}\""
    info "-------------------------------------------------------"

    OUTPUT_NAME="splayer-headless-${arch_name}"
    TARGET_BIN="${DIST_DIR}/${OUTPUT_NAME}"

    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=${target_cpu}" \
    DIRETTA_ARCH="${diretta_arch}" \
    cargo build --release --target x86_64-unknown-linux-gnu --package headless-server

    cp -f --remove-destination "${PROJECT_ROOT}/target/x86_64-unknown-linux-gnu/release/headless-server" "${TARGET_BIN}"
    strip --strip-all "${TARGET_BIN}" 2>/dev/null || true

    BIN_SIZE=$(du -h "${TARGET_BIN}" | cut -f1)
    success "架构 [${arch_name}] 构建完成: ${TARGET_BIN} (${BIN_SIZE})"
done

# 构建通用前端产物
info "-------------------------------------------------------"
info "正在构建 Web UI 生产静态资源..."
pnpm build:web 2>/dev/null || npx electron-vite build

mkdir -p "${DIST_DIR}/web"
cp -r "${PROJECT_ROOT}/out/renderer/"* "${DIST_DIR}/web/"
success "Web UI 资源已同步到 ${DIST_DIR}/web"

info "======================================================="
success "全部 CPU 微架构变体构建完成！产物列表："
ls -lh "${DIST_DIR}"
info "======================================================="
