#!/usr/bin/env bash
# Diretta headless 真机客观验收（T1/T2/T3 自动化部分）
# 用法: ./diretta-e2e-check.sh [BASE_URL] [媒体目录] [音轨A] [音轨B]
# 依赖: curl, python3, (可选)生成音轨用 python3 内置 wave 模块
set -u
BASE=${1:-http://127.0.0.1:14559}
MEDIA=${2:-/tmp/splayer-e2e/media}
TRACK_A=${3:-$MEDIA/trackA_440.wav}
TRACK_B=${4:-$MEDIA/trackB_880.wav}
PASS=0; FAIL=0

say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "  PASS: $*"; }
bad()  { FAIL=$((FAIL+1)); say "  FAIL: $*"; }
api()  { curl -s -m "$1" "${@:2}"; }
post() { api 90 "$BASE$1" -X POST -H 'Content-Type: application/json' -d "$2"; }
st()   { api 3 "$BASE/api/status" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d['state'],f\"{d['position']:.2f}\",f\"{d['duration']:.2f}\",d.get('current_source') or '')" 2>/dev/null; }
wait_playing() { for i in $(seq 1 30); do s=$(st); [ "${s%% *}" = "Playing" ] && return 0; sleep 1; done; return 1; }

gen_tracks() {
  mkdir -p "$MEDIA"
  python3 - "$MEDIA" <<'EOF'
import wave, math, struct, sys, os
media = sys.argv[1]; SR = 44100; CH = 2; SECS = 35
for name, f in {"trackA_440.wav": 440, "trackB_880.wav": 880}.items():
    p = os.path.join(media, name)
    if os.path.exists(p) and os.path.getsize(p) > 1000000: continue
    w = wave.open(p, "wb")
    w.setnchannels(CH)
    w.setsampwidth(2)
    w.setframerate(SR)
    frames = bytearray()
    for i in range(SR * SECS):
        v = int(12000 * math.sin(2 * math.pi * f * i / SR))
        frames += struct.pack("<hh", v, v)
    w.writeframes(bytes(frames))
    w.close()
    print("generated:", p)
EOF
  [ -s "$MEDIA/trackA_440.wav" ] && [ -s "$MEDIA/trackB_880.wav" ] || { say "FAIL: 测试音轨生成失败"; exit 1; }
}

# T1 基础播放：state=Playing 且 10 秒位置推进 ≥8s
t1() {
  say "== T1 基础播放 =="
  local resp
  resp=$(post /api/v1/player/load "{\"source\":\"$TRACK_A\",\"auto_play\":true}")
  if ! wait_playing; then
    say "  load 响应: ${resp:-<empty>}"
    bad "加载后未进入 Playing（响应即服务端错误文本，常见为 Target 未就绪——先断电重启 Target）"
    return 1
  fi
  p0=$(st | awk '{print $2}'); sleep 10; p1=$(st | awk '{print $2}')
  awk -v a="$p0" -v b="$p1" 'BEGIN { exit !((b - a) >= 8.0) }' \
    && ok "位置推进 ${p0}→${p1}（10s）" || bad "位置推进不足: ${p0}→${p1}"
}

# T2 切歌连续性 ×5：每次切换 ≤3s 回到 Playing，且无跳曲
t2() {
  say "== T2 切歌连续性 ×5 =="
  for n in 1 2 3 4 5; do
    src=$([ $((n % 2)) -eq 1 ] && echo "$TRACK_A" || echo "$TRACK_B")
    t0=$(date +%s)
    resp=$(post /api/v1/player/load "{\"source\":\"$src\",\"auto_play\":true}")
    if wait_playing; then
      dt=$(( $(date +%s) - t0 ))
      [ "$dt" -le 3 ] && ok "切换 #$n 完成（${dt}s）" || bad "切换 #$n 耗时 ${dt}s（>3s）"
    else
      say "  load 响应: ${resp:-<empty>}"
      bad "切换 #$n 未回到 Playing"
    fi
    sleep 2
  done
}

# T3 关闭前端自动接续：注册候选 → seek 到尾部 → 停止轮询 → 服务端自动接力
t3() {
  say "== T3 关闭前端自动接续 =="
  dur=$(st | awk '{print $3}'); target=$(python3 -c "print(max(0, $dur - 4))")
  post /api/v1/player/queue/next-candidate "{\"source\":\"$TRACK_B\",\"duration_hint\":35}" > /dev/null
  post /api/v1/player/seek "{\"position_secs\":$target}" > /dev/null
  say "  前端已离场（停止轮询 12s，模拟浏览器关闭）……"
  sleep 12
  s=$(st); state=${s%% *}; src=${s##* }
  if [ "$state" = "Playing" ] && [ "$src" = "$TRACK_B" ]; then
    ok "服务端自动接续到候选曲（B 层核心验收）"
  elif [ "$state" = "Paused" ]; then
    bad "候选接力失败进入 Paused（检查 autoAdvanceFailed 与服务端日志）"
  else
    bad "曲终后状态异常: $s"
  fi
  curl -s -X DELETE "${BASE}/api/v1/player/queue/next-candidate" > /dev/null 2>&1 || true
}

# T4 无缝边界不触发全量重连：gapless 边界消费前后 FullReconnect 日志计数不变
# （判据照抄 real-device-acceptance.md T4；日志源优先 journalctl，否则本机输出文件）
full_reconnect_count() {
  if command -v journalctl >/dev/null && systemctl is-active --quiet splayer-headless.service 2>/dev/null; then
    journalctl -u splayer-headless.service --since -"5 minutes" 2>/dev/null | grep -c "full_reconnect\|FullReconnect" || true
  elif [ -n "${LOG_FILE:-}" ] && [ -f "$LOG_FILE" ]; then
    grep -c "full_reconnect\|FullReconnect" "$LOG_FILE" || true
  else
    echo "-1"
  fi
}
t4() {
  say "== T4 无缝边界不触发 FullReconnect =="
  if [ "$(full_reconnect_count)" = "-1" ]; then
    say "  SKIP: 无可用日志源（设置 LOG_FILE=服务端日志路径 后重跑）"
    return
  fi
  post /api/v1/player/load "{\"source\":\"$TRACK_A\"}" > /dev/null
  wait_playing || { bad "T4 起播失败"; return; }
  before=$(full_reconnect_count)
  # 注册候选 → 推进到边界，等 stage(阈值 30s 内) + boundary 消费
  post /api/v1/player/queue/next-candidate "{\"source\":\"$TRACK_B\",\"duration_hint\":35}" > /dev/null
  dur=$(st | awk '{print $3}'); target=$(python3 -c "print(max(0, $dur - 4))")
  post /api/v1/player/seek "{\"position_secs\":$target}" > /dev/null
  sleep 12
  after=$(full_reconnect_count)
  s=$(st)
  if [ "$after" = "$before" ] && [ "${s%% *}" = "Playing" ]; then
    ok "gapless 边界消费未触发全量重连（count=$before→$after）"
  else
    bad "边界发生 FullReconnect（count=$before→$after）或状态异常: $s"
  fi
  curl -s -X DELETE "${BASE}/api/v1/player/queue/next-candidate" > /dev/null 2>&1 || true
}

# T5 30 分钟 RSS 平稳采样：每 60s 采样 /proc/<pid>/statm，末值不高于初值 +15%
# （判据照抄 real-device-acceptance.md T5；长测，RUN_T5=1 显式开启）
t5() {
  say "== T5 30 分钟 RSS 平稳采样 =="
  pid=$(pgrep -o -f splayer-headless || true)
  [ -n "$pid" ] || { bad "未找到服务进程"; return; }
  rss() { awk '{print int($2 * 4096 / 1048576)}' "/proc/$pid/statm" 2>/dev/null || echo 0; }
  post /api/v1/player/load "{\"source\":\"$TRACK_A\"}" > /dev/null
  wait_playing || { bad "T5 起播失败"; return; }
  start_rss=$(rss)
  say "  起始 RSS: ${start_rss} MiB，采样 30 次 × 60s ……"
  peak=$start_rss
  for i in $(seq 1 30); do
    sleep 60
    cur=$(rss)
    [ "$cur" -gt "$peak" ] && peak=$cur
    say "  [$i/30] RSS=${cur} MiB"
  done
  end_rss=$(rss)
  limit=$(( start_rss + start_rss * 15 / 100 ))
  if [ "$end_rss" -le "$limit" ] && [ "$peak" -le $(( limit + 50 )) ]; then
    ok "RSS 平稳（start=${start_rss} end=${end_rss} peak=${peak} MiB，limit=${limit}）"
  else
    bad "RSS 增长异常（start=${start_rss} end=${end_rss} peak=${peak} MiB，limit=${limit}）"
  fi
}

# T6 断链恢复：拔除 Diretta 链路 → 服务应报告 OutputStalled/失败并进入恢复；
# 重新连接 → 自动恢复播放（判据照抄 real-device-acceptance.md T6，半自动引导）
t6() {
  say "== T6 断链恢复（半自动） =="
  post /api/v1/player/load "{\"source\":\"$TRACK_A\"}" > /dev/null
  wait_playing || { bad "T6 起播失败"; return; }
  printf '  请拔除 Target 网线/断电，等待 ≤60s 后按回车继续……'
  read -r
  stalled=0
  for i in $(seq 1 90); do
    if api 3 "$BASE/api/status" | grep -q "Stalled\\|outputStalled"; then stalled=1; break; fi
    journalctl -u splayer-headless.service --since -"2 minutes" 2>/dev/null | grep -q "OutputStalled\\|output.*恢复" && { stalled=1; break; }
    sleep 1
  done
  [ "$stalled" = "1" ] && ok "断链后输出停滞已被检测" || bad "断链后未见停滞上报"
  printf '  请恢复链路（重新上电/插回网线），等待自动恢复后按回车……'
  read -r
  if wait_playing; then
    ok "链路恢复后自动回到 Playing"
  else
    bad "链路恢复后 30s 内未回到 Playing"
  fi
}

gen_tracks
t1; t2; t3; t4
[ "${RUN_T5:-0}" = "1" ] && t5
[ "${RUN_T6:-0}" = "1" ] && t6
say "=============================="
say "结果: PASS=$PASS FAIL=$FAIL"
exit $((FAIL > 0))
