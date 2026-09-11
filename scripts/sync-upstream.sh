#!/usr/bin/env bash
# 获取上游并列出待审核变更；合并在独立工作区人工执行。
set -euo pipefail

REMOTE="${1:-origin}"
BRANCH="${2:-dev}"
[[ $# -le 2 && "$REMOTE" != -* && "$BRANCH" != -* ]] || {
  printf 'Usage: bash scripts/sync-upstream.sh [remote] [branch]\n' >&2
  exit 2
}
cd "$(git rev-parse --show-toplevel)"
git remote get-url "$REMOTE" >/dev/null
git check-ref-format "refs/heads/$BRANCH"
if git rev-parse -q --verify MERGE_HEAD >/dev/null; then
  printf 'An existing merge must be reviewed before preparing another sync.\n' >&2
  exit 2
fi
git fetch --no-tags "$REMOTE" "refs/heads/$BRANCH:refs/remotes/$REMOTE/$BRANCH"
UPSTREAM="$(git rev-parse "refs/remotes/$REMOTE/$BRANCH")"
BASE="$(git merge-base HEAD "$UPSTREAM")"
printf 'Product: %s\nUpstream: %s\nMerge base: %s\n' "$(git rev-parse HEAD)" "$UPSTREAM" "$BASE"
printf '\nProduct-only / upstream-only commit counts:\n'
git rev-list --left-right --count "HEAD...$UPSTREAM"
printf '\nUpstream commits requiring review:\n'
git log --oneline "HEAD..$UPSTREAM"
printf '\nUpstream paths requiring review:\n'
git diff --name-status "$BASE" "$UPSTREAM"
printf '\nReview native/audio-engine changes against native/audio-engine-core.\n'
printf 'Review Electron business logic against Headless implementations.\n'
printf 'See docs/linux-headless-server/maintenance-baseline.md for integration gates.\n'
printf 'Preparation complete; no merge, checkout, staging or commit was performed.\n'
