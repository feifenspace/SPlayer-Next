# Diretta 真机验收 SOP（批次 E/F + F11 + B 层）

> 适用：`splayer-headless-linux-x86_64-v2-diretta-sdk148-20260905` 及之后的构建。
> 原则：客观判据优先（API 状态 + WS 事件 + journalctl），人耳听感只做最后抽检。
> 配套脚本：`scripts/diretta-e2e-check.sh`（自动执行 T1/T2/T3 的客观部分并输出 PASS/FAIL）。

## 0. 前置

1. 部署新包并重启服务：`sudo ./install.sh`（或按现有 systemd 方式），确认 `journalctl -u splayer-headless -f` 无 ERROR。
2. **Target 预处理（重要）**：若此前有服务进程被强杀，Target（Combo384/TargetApp）可能残留旧会话导致"握手成功但永不消费"（本次远程测试已复现该状态：scan 正常、握手成功、能力查询挂起 30s、播放 0 消费）。先给 DAC/TargetApp **断电 30 秒再上电**。
3. 确认音量已压低（正弦/测试信号），`POST /player/volume {"volume":0.15}`。
4. 准备 3 首同格式本地测试音轨（44.1k/16bit/stereo WAV，35 秒，不同频率正弦），放入服务器本地目录。

自动化脚本会自行生成音轨（本机有 python3 即可）。

## T1｜基础播放与位置推进（批次 E 回归底线）

```bash
curl -X POST $BASE/api/v1/player/load -H 'Content-Type: application/json' \
  -d '{"source":"/绝对路径/trackA.wav","auto_play":true}'
# 每 1s 采样一次 /api/status，共 10 次
```

PASS 判据：
- state=Playing 且 position 每 秒推进 ≈1.0（10 秒累计 ≥9s，漂移 <1s）；
- 期间 journalctl 无 `OutputStalled` / `SCHED_FIFO 设置失败`（后者说明服务缺 RT 权限）；
- 无 `outputFailed`。

## T2｜切歌连续性 ×5（数字侧无爆音代理验证）

循环 5 次：播放 A → 第 25 秒 `load` B → 等 5 秒 → 切回 A。

每次切换 PASS 判据（客观）：
- 切换完成 ≤3s（load 请求返回到 state=Playing）；
- 切换前后 ±3s 窗口内 **无 OutputStalled / sourceError / outputRecoveryFailed**；
- 新曲 position 从 0 起推进。

**爆音听感抽检**（需人在场）：数字侧连续只保证没有"供数空窗"，模拟域的最后一级仍需人耳：切歌瞬间听音箱——干净无声过渡 = PASS；有"啪"声 = FAIL（记录当时 journalctl 的 `pre-mute`/`boundary` 时序交回分析）。

## T3｜关闭前端自动接续（B 层核心验收）

1. 播放 A，曲中注册候选：
   `POST /api/v1/player/queue/next-candidate {"source":"<B 的路径>","duration_hint":35}`
2. **关闭浏览器**（或杀掉所有 WS 客户端，确认 `ss -tnp | grep 14559` 无 ESTABLISHED）。
3. seek 到 `duration - 4s`，等待自然播完。

PASS 判据：
- 曲终后 ≤2s，服务端自动加载 B：`/api/status` 的 current_source 变为 B 且 state=Playing；
- journalctl 出现 `曲终自动连播：加载下一曲候选`；
- WS 重连后收到 `nextCandidateChanged`（注册/消费通知）。

边界用例：
- 不注册候选 → 播完即停（state=Stopped）；
- repeatMode=one → 本地 seek(0)+play 续播（不走服务端接力）；
- 候选加载失败 → state=Paused + `autoAdvanceFailed` 事件。

## T4｜Direct 无缝 boundary（无 SDK 重建）

前置：Diretta 输出 + 同格式两曲。曲中（剩余 >30s 前）由前端/脚本调 `POST /player/direct/stage_next`。

PASS 判据：
- WS 收到 `directTrackBoundary`（duration/generation 正确）；
- boundary 前后 position 连续（无 >300ms 停顿、无回跳）；
- journalctl 无 `FullReconnect`（连接保持，未拆线）。

## T5｜长播稳定性（30 分钟）

播一张长专辑循环：PASS = 无误报 `OutputStalled`（误报 = pre-mute/阈值参数需要回调）、无恢复循环、RSS 平稳（`ps -o rss -p <pid>` 采样）。

## T6｜F11 断链恢复（破坏性，需在场）

播放中拔网线/关 Target 电源 ≥10s 再恢复：
- PASS：恢复后 ≤5s 内自动重载续播；持续断链 15s+ 进入 Paused 并广播 `outputRecoveryFailed`；Target 恢复后可继续。

## 已知问题（本次远程测试实测）

- **Target 半响应状态**：`TargetApp_5DCC` 在宿主被强杀后可能进入"扫描可见、握手成功、但永不消费"状态，且能力查询会挂起 30s+。处置：Target 断电重启。此状态与代码版本无关（新旧参数均复现）。
- **非 RT 环境运行**：手动运行（无 systemd 授权）时 `SCHED_FIFO` 降级，Diretta Target 可能因 host 周期抖动拒绝锁定——真机验收务必走 systemd（`LimitRTPRIO/AmbientCapabilities` 已在 unit 内配置）。
