#!/usr/bin/env bash
# Linux 实时音频系统调优（蓝图《Linux Headless Hi-Fi.md》§四）
#
# 用途：为 headless Hi-Fi 主机生成本机拓扑定制的实时调优配置：
#   1. GRUB 内核参数：isolcpus/nohz_full/rcu_nocbs 隔离核、CPU 空闲降级关闭、
#      audit 关闭（§四：音频隔离核不受内核后台任务与审计中断干扰）
#   2. /etc/security/limits.d/99-hifi-audio.conf：rtprio/memlock/nice 上限
#   3. IRQ 亲和：默认把中断批量赶到 Core 0，隔离核零中断（--keep-irq 白名单除外）
#
# 安全纪律（优化方案 §3-E.1）：
#   - 默认 --dry-run，只打印将执行的变更 diff；--apply 才落盘
#   - GRUB 改动要求交互输入 yes 确认，且自动备份原文件（带时间戳）
#   - 脚本幂等：重复运行产出相同配置，已存在且一致的文件跳过
set -euo pipefail

DRY_RUN=1
KEEP_IRQ_SERVICES=""
ISOLATED_OVERRIDE=""

log()  { printf '[rt-tuning] %s\n' "$*"; }
warn() { printf '[rt-tuning] 警告: %s\n' "$*" >&2; }

usage() {
  cat <<EOF
用法: $0 [--apply] [--isolated <cpulist>] [--keep-irq <driver1,driver2>]

  --apply            实际落盘（默认 dry-run 仅打印）
  --isolated <list>  显式指定隔离核（如 "2-3" 或 "2,3"），缺省自动取一半物理核
  --keep-irq <list>  不迁移的中断驱动白名单（逗号分隔，如 "xhci_hcd,alsa"）

安全: GRUB 修改需交互确认并自动备份；除 --apply 外不触碰系统。
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply) DRY_RUN=0 ;;
    --isolated) ISOLATED_OVERRIDE="$2"; shift ;;
    --keep-irq) KEEP_IRQ_SERVICES="$2"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 1 ;;
  esac
  shift
done

[[ $EUID -eq 0 ]] || { warn "请以 root 运行（limits.d/GRUB/IRQ 均需 root）"; exit 1; }

# ── 1. 拓扑探测与隔离核选择 ────────────────────────────────────────────
NPROC=$(nproc)
PHYSICAL_CORES=$(grep -c ^processor /proc/cpuinfo)

# 取高半区物理核作为隔离核（典型 8 核机器隔离 4-7；4 核隔离 2-3）
HALF=$(( PHYSICAL_CORES / 2 ))
AUTO_ISOLATED="${HALF}-$(( PHYSICAL_CORES - 1 ))"
ISOLATED="${ISOLATED_OVERRIDE:-$AUTO_ISOLATED}"

log "CPU 逻辑核: $NPROC，物理核: $PHYSICAL_CORES"
log "隔离核方案: $ISOLATED（自动方案为高半区物理核，可用 --isolated 覆盖）"

# ── 2. GRUB 内核参数 ──────────────────────────────────────────────────
GRUB_DEFAULT="/etc/default/grub"
GRUB_KEY="GRUB_CMDLINE_LINUX_DEFAULT"
NEEDED_PARAMS=(
  "isolcpus=nohz_full,domain,managed_irq,${ISOLATED}"
  "nohz_full=${ISOLATED}"
  "rcu_nocbs=${ISOLATED}"
  "processor.max_cstate=1"
  "intel_idle.max_cstate=1"
  "idle=poll"
  "audit=0"
)

current_grub_line=$(grep -E "^${GRUB_KEY}=" "$GRUB_DEFAULT" 2>/dev/null | tail -1 || true)
log "当前 GRUB 行: ${current_grub_line:-<未找到>}"

missing_params=()
for p in "${NEEDED_PARAMS[@]}"; do
  base="${p%%=*}"
  grep -qE "(^|[[:space:]])${base}=" <<<"${current_grub_line:-}" || missing_params+=("$p")
done

if [[ ${#missing_params[@]} -eq 0 ]]; then
  log "GRUB 参数已齐备，跳过"
else
  log "将追加内核参数: ${missing_params[*]}"
  new_grub_line="${current_grub_line:-${GRUB_KEY}=\"\"}"
  new_grub_line="${new_grub_line%\"}${missing_params[*]}\""
  apply_grub() {
    cp "$GRUB_DEFAULT" "$GRUB_DEFAULT.bak.$(date +%Y%m%d%H%M%S)"
    if grep -qE "^${GRUB_KEY}=" "$GRUB_DEFAULT"; then
      sed -i "s|^${GRUB_KEY}=.*|${new_grub_line}|" "$GRUB_DEFAULT"
    else
      echo "$new_grub_line" >> "$GRUB_DEFAULT"
    fi
    if command -v update-grub >/dev/null; then update-grub
    elif command -v grub2-mkconfig >/dev/null; then grub2-mkconfig -o /boot/grub2/grub.cfg
    fi
    log "GRUB 已更新（重启后生效）。回滚: 恢复 $GRUB_DEFAULT.bak.* 后重新 update-grub"
  }
  if [[ $DRY_RUN -eq 1 ]]; then
    log "[dry-run] 将改写 ${GRUB_KEY} 为:"
    log "[dry-run]   $new_grub_line"
  else
    read -r -p "[rt-tuning] GRUB 改动影响全局启动参数，重启后生效。确认写入？输入 yes 继续: " reply
    [[ $reply == "yes" ]] || { warn "已取消 GRUB 修改"; }
    if [[ "${reply:-}" == "yes" ]]; then apply_grub; fi
  fi
fi

# ── 3. limits.d ───────────────────────────────────────────────────────
LIMITS_FILE="/etc/security/limits.d/99-hifi-audio.conf"
LIMITS_CONTENT="\
# SPlayer-Next Headless 实时音频上限（rt-tuning.sh 生成）
*    soft    rtprio   99
*    hard    rtprio   99
*    soft    memlock  unlimited
*    hard    memlock  unlimited
*    soft    nice     -20
*    hard    nice     -20
"
apply_limits() {
  printf '%s' "$LIMITS_CONTENT" > "$LIMITS_FILE"
  log "已写入 $LIMITS_FILE"
}
if [[ -f $LIMITS_FILE && "$(cat "$LIMITS_FILE")" == "$LIMITS_CONTENT" ]]; then
  log "limits.d 已是目标内容，跳过"
elif [[ $DRY_RUN -eq 1 ]]; then
  log "[dry-run] 将写入 $LIMITS_FILE:"
  printf '%s\n' "$LIMITS_CONTENT" | sed 's/^/[dry-run]   /'
else
  apply_limits
fi

# ── 4. IRQ 亲和（运行时，非持久；持久化建议由 systemd unit 或 tuned 提供）──
apply_irq() {
  local migrated=0
  for irq_dir in /proc/irq/[0-9]*; do
    local irq=${irq_dir##*/}
    [[ $irq -eq 0 ]] && continue   # IRQ0 保留
    local consumers
    consumers=$(cat "$irq_dir"/*_affinity_hint 2>/dev/null || true)
    local service
    service=$(basename "$(readlink -f "$irq_dir"/* 2>/dev/null | head -1)" 2>/dev/null || true)
    local skip=0
    if [[ -n $KEEP_IRQ_SERVICES ]]; then
      IFS=',' read -ra keep_list <<< "$KEEP_IRQ_SERVICES"
      for keep in "${keep_list[@]}"; do
        if grep -ql "$keep" "$irq_dir"/../irq/"$irq"/* 2>/dev/null \
           || ls /sys/kernel/irq/"$irq"/actions 2>/dev/null | grep -q "$keep"; then
          skip=1; break
        fi
      done
      [[ $skip -eq 1 ]] && continue
    fi
    # 绑到 Core 0（掩码 0x1）——隔离核零中断
    if echo 1 > "$irq_dir/smp_affinity" 2>/dev/null; then
      migrated=$((migrated+1))
    fi
  done
  log "IRQ 亲和：$migrated 个中断已迁出隔离核（掩码 0x1，白名单: ${KEEP_IRQ_SERVICES:-无}）"
}
if [[ $DRY_RUN -eq 1 ]]; then
  log "[dry-run] 将把可迁移中断的 smp_affinity 设为 0x1（Core 0），白名单: ${KEEP_IRQ_SERVICES:-无}"
  log "[dry-run] 声卡中断请加入白名单避免误迁（如 --keep-irq alsasvc）"
else
  apply_irq
fi

# ── 5. 校验提示 ───────────────────────────────────────────────────────
log "完成。验收命令（§六.3）："
log "  cyclictest -p 80 -t 1 -a ${ISOLATED%%-*} -n -i 200 -l 1000000  # Max ≤ 15μs"
log "  cat /sys/devices/system/cpu/isolated                            # 应显示 $ISOLATED"
[[ $DRY_RUN -eq 1 ]] && log "以上为 dry-run 预览；确认无误后加 --apply 执行。"
exit 0
