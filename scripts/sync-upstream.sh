#!/usr/bin/env bash
# ==================================================================
# SPlayer-Next-Headless 上游同步脚本
# ------------------------------------------------------------------
# 用途：一键同步上游分支，策略化处理 headless fork 已接管的区域，
#       最大限度做到“零手工修复”：
#         1. native/audio-engine/**  —— fork 已接管（实现迁移至
#            audio-engine-core），冲突一律保留本地版本；
#         2. Cargo.lock              —— 保留本地版本，稍后 cargo update -w；
#         3. 其余冲突                —— 依赖 git rerere 自动重放历史
#            解法；首次遇到的会列出并退出，人工解决一次后永久记住。
#
# 用法：bash scripts/sync-upstream.sh [remote] [branch]
#   remote  默认 origin
#   branch  默认 dev
#   SKIP_CHECKS=1 跳过合并后的 typecheck / cargo check
#
# 脚本可重入：手工解决完冲突后直接重跑，会继续完成合并提交。
# ==================================================================
set -euo pipefail

REMOTE="${1:-origin}"
BRANCH="${2:-dev}"
GIT_DIR="$(git rev-parse --git-dir)"
cd "$(git rev-parse --show-toplevel)"

info() { printf "\033[1;34m[sync]\033[0m %s\n" "$*"; }
warn() { printf "\033[1;33m[sync]\033[0m %s\n" "$*"; }
fail() { printf "\033[1;31m[sync]\033[0m %s\n" "$*" >&2; exit 1; }

# ---- 0. 兜底配置 merge 驱动与 rerere（幂等） ----------------------
git config merge.ours.name "Keep our version (headless fork)"
git config merge.ours.driver "cat %A"
git config rerere.enabled true
git config rerere.autoupdate true

# ---- 1. 发起合并（或续接未完成的合并） ----------------------------
if [[ -f "$GIT_DIR/MERGE_HEAD" ]]; then
  info "检测到进行中的合并，继续处理冲突 ..."
else
  # 合并要求工作树干净（忽略未跟踪文件，不影响合并操作）
  if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    warn "工作树存在未提交改动，请先提交后再同步："
    git status --short --untracked-files=no
    exit 1
  fi

  info "拉取 ${REMOTE}/${BRANCH} ..."
  git fetch "$REMOTE"
  git rev-parse --verify -q "${REMOTE}/${BRANCH}" >/dev/null ||
    fail "远程分支 ${REMOTE}/${BRANCH} 不存在，请检查参数。"

  BEHIND="$(git rev-list --count "HEAD..${REMOTE}/${BRANCH}")"
  if [[ "$BEHIND" -eq 0 ]]; then
    info "已是最新，无需同步。"
    exit 0
  fi
  info "上游领先 ${BEHIND} 个提交，开始合并 ..."
  git merge --no-edit "${REMOTE}/${BRANCH}" &&
    info "合并完成，无冲突。" || true
fi

# ---- 2. 策略化解决冲突 --------------------------------------------
if [[ -f "$GIT_DIR/MERGE_HEAD" ]]; then
  info "按 fork 策略自动处理接管区域 ..."
  REMAIN=""
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    st="$(git status --porcelain -- "$f" | cut -c1-2)"
    case "$f" in
      native/audio-engine/*)
        # fork 已接管：DU（上游改了本地已删的文件）保持删除，
        # 其余（UU/UD/AA）保留本地版本
        if [[ "$st" == "DU" ]]; then
          git rm -q -- "$f"
        else
          git add -- "$f"
        fi
        ;;
      Cargo.lock)
        # 保留本地版本，稍后由 cargo update -w 修正依赖图
        git checkout --ours -- "$f" && git add -- "$f"
        warn "Cargo.lock 保留本地版本，合并完成后建议执行：cargo update -w"
        ;;
      *)
        REMAIN+="$f"$'\n'
        ;;
    esac
  done <<< "$(git diff --name-only --diff-filter=U)"

  # rerere 已自动重放并 stage 的冲突不会出现在 U 列表；
  # 这里剩下的均为首次遇到、需要人工介入的冲突。
  if [[ -n "$REMAIN" ]]; then
    warn "以下冲突无法自动处理（首次遇到），请手工解决后 git add，再重跑本脚本："
    printf '%s' "$REMAIN"
    warn "提示：人工解决一次后 rerere 会记住方案，下次同类冲突将自动重放。"
    exit 2
  fi

  git commit --no-edit
  info "合并提交完成：$(git rev-parse --short HEAD)"
fi

# ---- 3. 质量检查 ---------------------------------------------------
if [[ "${SKIP_CHECKS:-0}" == "1" ]]; then
  info "SKIP_CHECKS=1，跳过质量检查。"
  info "同步完成。"
  exit 0
fi

info "运行 pnpm typecheck ..."
pnpm typecheck

if command -v cargo >/dev/null 2>&1; then
  info "运行 cargo check（headless-server + audio-engine-core）..."
  cargo check -p headless-server -p audio-engine-core --quiet
fi

info "上游同步完成，全部检查通过。"
