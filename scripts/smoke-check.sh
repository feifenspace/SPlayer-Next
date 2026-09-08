#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# SPlayer headless-server 冒烟回归门（全面检查修复计划 批次 0.2）
#
# 用真实二进制起临时实例（127.0.0.1 + 随机端口 + 隔离数据目录），验证：
#   [P0] 封面接口路径穿越金丝雀（批次 1.1 修复项）
#   [P0] CLI --host/--port 无条件覆盖配置文件（批次 1.4 修复项）
#   [P0] WS 握手与 snapshot 推送
#   [P2] CORS 预检允许 PUT（批次 2.4 修复项，当前预期 WARN）
#   [P2] SIGTERM 优雅关闭（批次 5.4 修复项，当前预期 WARN）
#
# 用法: scripts/smoke-check.sh [--bin PATH] [--keep]
# 退出码: 0 = 关键项全部通过; 1 = 有关键项失败
# ---------------------------------------------------------------------------
set -u
cd "$(dirname "$0")/.."

BIN="target/debug/headless-server"
KEEP=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin) BIN="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    *) echo "未知参数: $1"; exit 2 ;;
  esac
done

PASS=0; FAIL=0; WARN=0
TMP="$(mktemp -d /tmp/splayer-smoke.XXXXXX)"
PIDS=()

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; done
  [[ "$KEEP" == "1" ]] || rm -rf "$TMP"
}
trap cleanup EXIT

ok()   { echo "PASS: $*"; PASS=$((PASS+1)); }
bad()  { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
warn() { echo "WARN: $* (计划内待修复项)"; WARN=$((WARN+1)); }

pick_port() {
  python3 - <<'EOF'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
EOF
}

wait_up() { # $1=url  $2=超时秒
  for _ in $(seq 1 $((${2:-15} * 2))); do
    curl -sf -m 1 "$1" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

bound_addr() { # $1=logfile → 输出实际监听地址
  grep -oP 'listening on \K[0-9a-fA-F.:]+(?:\[\.\.\])?' "$1" 2>/dev/null | head -1
}

[[ -x "$BIN" ]] || { echo "FAIL: 二进制不存在: $BIN（先 cargo build -p headless-server）"; exit 1; }

# ---------- 实例 1：显式 CLI 参数 + 隔离数据目录 ----------
PORT_REQ=$(pick_port)
"$BIN" --host 127.0.0.1 --port "$PORT_REQ" --data-dir "$TMP/data" >"$TMP/server.log" 2>&1 &
PID=$!
PIDS+=("$PID")

# 实际绑定地址以日志为准（CLI 覆盖失效时与请求端口不同，本身就是一个检查项）
if ! wait_up "http://127.0.0.1:${PORT_REQ}/api/status" 15; then
  ACTUAL="$(bound_addr "$TMP/server.log")"
  echo "FAIL: 实例未在请求端口 ${PORT_REQ} 就绪（日志实际绑定: ${ACTUAL:-无}）"
  echo "----- server.log -----"; tail -20 "$TMP/server.log"
  exit 1
fi
ok "实例启动并监听 127.0.0.1:${PORT_REQ}"

# CLI 覆盖（P0，批次 1.4）
if [[ "$(bound_addr "$TMP/server.log")" == "127.0.0.1:${PORT_REQ}" ]]; then
  ok "CLI --host/--port 覆盖配置文件生效"
else
  bad "CLI --host/--port 被配置文件 listen_addr 覆盖（批次 1.4）"
fi

# /api/status 契约
STATUS=$(curl -sf -m 3 "http://127.0.0.1:${PORT_REQ}/api/status" || true)
if echo "$STATUS" | grep -q '"protocol":{"version":2}'; then
  ok "/api/status 协议版本 2"
else
  bad "/api/status 响应异常: ${STATUS:0:120}"
fi

# ---------- 路径穿越金丝雀（P0，批次 1.1） ----------
mkdir -p "$TMP/data/covers"
echo "SMOKE-CANARY-DO-NOT-READ" > "$TMP/data/canary.txt"
TRAV=$(curl -s -m 3 --path-as-is \
  "http://127.0.0.1:${PORT_REQ}/api/v1/covers/..%2Fcanary.txt" || true)
TRAV2=$(curl -s -m 3 --path-as-is \
  "http://127.0.0.1:${PORT_REQ}/api/v1/covers/..%2F..%2F..%2Fetc%2Fpasswd" || true)
if [[ "$TRAV$TRAV2" != *"SMOKE-CANARY"* && "$TRAV2" != "root:"* ]]; then
  ok "封面接口路径穿越被封堵"
else
  bad "封面接口路径穿越仍可读取（批次 1.1）"
fi

# ---------- WebSocket 握手与 snapshot ----------
if node - "$PORT_REQ" <<'EOF' 2>"$TMP/ws.err"; then
const WebSocket = require("ws");
const ws = new WebSocket(`ws://127.0.0.1:${process.argv[2]}/ws`);
const t = setTimeout(() => { console.error("ws snapshot 超时"); process.exit(1); }, 5000);
ws.on("message", (raw) => {
  let m; try { m = JSON.parse(raw); } catch { return; }
  if (m.type === "snapshot") { clearTimeout(t); ws.close(); process.exit(0); }
});
ws.on("error", () => { clearTimeout(t); process.exit(1); });
ws.on("close", (code) => { clearTimeout(t); process.exit(code === 1000 ? 0 : 1); });
EOF
  ok "WS 握手并收到 snapshot"
else
  bad "WS 握手/snapshot 失败: $(head -2 "$TMP/ws.err" 2>/dev/null)"
fi

# ---------- CORS 预检（批次 2.4，当前 WARN） ----------
ALLOW=$(curl -s -m 3 -X OPTIONS \
  -H "Origin: http://localhost:5173" -H "Access-Control-Request-Method: PUT" \
  -D - -o /dev/null "http://127.0.0.1:${PORT_REQ}/api/v1/player/queue" \
  | grep -i "access-control-allow-methods" || true)
if echo "$ALLOW" | grep -q "PUT"; then
  ok "CORS 预检允许 PUT"
else
  warn "CORS 预检 allow-methods 不含 PUT（批次 2.4）: ${ALLOW:-无头}"
fi

# ---------- SIGTERM 优雅关闭（批次 5.4，当前 WARN） ----------
kill -TERM "$PID" 2>/dev/null
wait "$PID"; CODE=$?
PIDS=()
case "$CODE" in
  0) ok "SIGTERM 优雅关闭（退出码 0）" ;;
  143) warn "SIGTERM 默认终止，无优雅关闭（批次 5.4，退出码 143）" ;;
  *) bad "SIGTERM 后异常退出码: $CODE" ;;
esac

echo "----------------------------------------"
echo "结果: PASS=${PASS} FAIL=${FAIL} WARN=${WARN}"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
