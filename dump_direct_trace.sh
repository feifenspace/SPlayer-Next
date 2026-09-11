#!/bin/bash
# dump_direct_trace.sh — 提取 Diretta handoff 路径的结构化日志
# 用法: ./dump_direct_trace.sh [/path/to/logfile]
#   不传参数则用 journalctl (systemd 环境) 或 tail -f /var/log/...

set -e

LOGFILE="${1:-}"

# ── 1. 找到进程 PID (假设是 audio-engine / splayer 进程)
find_pid() {
    PID=$(pgrep -f "audio-engine\|splayer\|splayer-next" | head -1)
    if [ -z "$PID" ]; then
        echo "ERROR: 找不到 audio-engine / splayer 进程" >&2
        exit 1
    fi
    echo "进程 PID: $PID"
}

# ── 2. 提取 diretta_handoff target 的日志行
extract_trace() {
    local src="$1"
    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo "  Diretta Handoff Trace (target=diretta_handoff)"
    echo "  来源: $src"
    echo "═══════════════════════════════════════════════════════════════"
    echo ""

    if [ -n "$LOGFILE" ]; then
        grep 'diretta_handoff' "$LOGFILE" | \
            awk '{
                # 解析结构化字段
                phase="?"
                source=""
                sample_rate=""
                channels=""
                silence_before=""
                silence_after=""
                format=""
                is_handoff=""
                consumed=""
                transition=""
                duration=""
                auto_play=""
                for (i=1;i<=NF;i++) {
                    if ($i ~ /^phase=/)   { sub(/^phase=/,"",$i); phase=$i }
                    if ($i ~ /^source=/)  { sub(/^source=/,"",$i); source=$i }
                    if ($i ~ /^sample_rate=/) { sub(/^sample_rate=/,"",$i); sample_rate=$i }
                    if ($i ~ /^channels=/) { sub(/^channels=/,"",$i); channels=$i }
                    if ($i ~ /^silence_blocks_before=/) { sub(/^silence_blocks_before=/,"",$i); silence_before=$i }
                    if ($i ~ /^silence_blocks_after=/)  { sub(/^silence_blocks_after=/,"",$i); silence_after=$i }
                    if ($i ~ /^format=/)  { sub(/^format=/,"",$i); format=$i }
                    if ($i ~ /^is_handoff_path=/) { sub(/^is_handoff_path=/,"",$i); is_handoff=$i }
                    if ($i ~ /^consumed_before=/) { sub(/^consumed_before=/,"",$i); consumed=$i }
                    if ($i ~ /^transition_count=/) { sub(/^transition_count=/,"",$i); transition=$i }
                    if ($i ~ /^duration=/)  { sub(/^duration=/,"",$i); duration=$i }
                    if ($i ~ /^auto_play=/) { sub(/^auto_play=/,"",$i); auto_play=$i }
                }
                printf "[%s] %-30s | src=%s | rate=%s | ch=%s | silence %s→%s | tx=%s | consumed=%.3f | dur=%.1fs | handoff=%s | auto=%s\n",
                       $1, phase, source, sample_rate, channels, silence_before, silence_after,
                       transition, consumed, duration, is_handoff, auto_play
            }'
    else
        # systemd journal 环境
        if command -v journalctl &>/dev/null; then
            journalctl -f --since "2 minutes ago" -o cat 2>/dev/null | \
                grep 'diretta_handoff' | while IFS= read -r line; do
                echo "$line"
            done
        else
            echo "ERROR: 未指定日志文件且无 journalctl，请传 LOGFILE 参数" >&2
            exit 1
        fi
    fi
}

# ── 3. 主逻辑
if [ -n "$LOGFILE" ]; then
    if [ ! -f "$LOGFILE" ]; then
        echo "ERROR: 日志文件不存在: $LOGFILE" >&2
        exit 1
    fi
    echo "从文件读取: $LOGFILE"
    extract_trace "$LOGFILE"
else
    find_pid
    echo "从 journalctl 实时监听 (Ctrl-C 退出) ..."
    echo "提示: 先执行测试切歌操作，再用本脚本抓取日志"
    extract_trace "journalctl"
fi
