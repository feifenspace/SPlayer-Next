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
  post /api/v1/player/queue/next-candidate-cancel > /dev/null 2>&1 || true
}

gen_tracks
t1; t2; t3
say "=============================="
say "结果: PASS=$PASS FAIL=$FAIL"
exit $((FAIL > 0))
