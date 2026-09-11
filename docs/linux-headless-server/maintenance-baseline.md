# Headless 维护基线与实施记录

后续构建整理见 [Web / Headless 构建](headless-build.md)：网易云依赖已改为固定 Git 提交，增加 Web 专用构建与 Headless CI。本文件以下保留第一批快照时的历史记录。

## 产品范围

交付 Web + Linux Headless，保留现有新增功能。Electron 源码暂作上游对照及兼容参考；不以桌面发布为本产品验收目标。

## 2026-09-11 快照

- 产品提交：`03e1af3f047206517074c51848ca543feffba1a9`，检查前工作区干净。
- 快照标签：`headless-snapshot-20260911-03e1af3`。这是源码快照，不代表真机验收通过。
- 整理分支：`maintenance/headless-baseline`。
- 整理工作区：`/home/songlian/SPlayer-Next-Headless-maintenance`。
- 原运行源码目录：`/home/songlian/SPlayer-Next-Headless`，维持 `dev`。
- 上游：`origin` = SPlayer-Dev/SPlayer-Next；产品远端：`fork` = feifenspace/SPlayer-Next。
- 已 fetch 的上游：`dc8eab18496faa41d5c8e72d12927b5fcb29e845`。
- 共同基点：`ac0bcfa1f70c54cadc046aba6472c8d13294c719`。
- 相对上述上游，产品独有 128 个提交、上游独有 14 个提交（包含合并提交）。

## 外部依赖

- `native/headless-server/Cargo.toml` 使用绝对路径 `/home/songlian/ncm-api-rs`。
- 该仓库版本为 `133b65bfe482e41ebccf018870d3fce07bf58eb3`，检查时无未提交改动。
- 本机 Diretta SDK：`/home/songlian/DirettaHostSDK_150`。SDK 不应在未经核对分发条件时直接复制入源码仓库。
- Rust 非交互 SSH 需将 `/home/songlian/.cargo/bin` 加入 PATH。
- 打包脚本默认输出目录也包含 `/home/songlian`，需在可复现交付批次参数化。
- 当前未更换依赖来源；应先核实 ncm 仓库来源及固定版本的可获取性，再使用固定 Git revision 或明确的依赖初始化流程。

## 验证记录

- `pnpm typecheck:web`：通过。
- `pnpm test:web`：12 个文件、74 项测试通过。日志含本地 14558/3000 端口连接失败及回退提示，需后续改善测试隔离；不算在线音源集成验收。
- `PATH=/home/songlian/.cargo/bin:$PATH DIRETTA_SDK_DIR=/home/songlian/DirettaHostSDK_150 cargo check --locked --offline -p headless-server`：通过（2 分 52 秒）。现有警告为 `alsa_mmap_sink.rs` 的 worker_paused/多余 mut，以及 `player.rs` 的 cover_dir；不代表真机播放通过。
- 同步准备脚本：`bash -n`、实际运行、`git diff --check` 通过；运行后 HEAD 仍为原提交，仅预期维护文件发生变更。
- `.gitattributes` 修改后音频文件 `merge` 属性为 unspecified。
- 未执行真机音频测试、在线登录测试或部署重启。

## 功能与代码归属

| 范围                  | 当前位置                                                | 维护方式                                                                          |
| --------------------- | ------------------------------------------------------- | --------------------------------------------------------------------------------- |
| 上游 UI 与产品新增 UI | src/components、src/pages、src/stores、src/apis         | 按功能审核合并；不可按目录整块覆盖                                                |
| 播放传输适配          | src/services/client                                     | 完善已有 IPlayerClient、HttpPlayerClient；保留兼容桥并逐步缩小范围                |
| Web 流媒体与认证      | src/services/streaming/web、src/stores/streamingAuth.ts | 保留 Subsonic/Jellyfin 等浏览器实现，核对上游业务修复                             |
| 类型与默认配置        | shared、src/settings                                    | 按接口契约审核，防止新字段只改一端                                                |
| 公共音频与输出扩展    | native/audio-engine-core                                | 对照上游 native/audio-engine；保留 ALSA/DSD/DoP/Diretta、SACD/CUE、RAM 播放等扩展 |
| NAPI 垫片             | native/audio-engine                                     | 保持与 core 的调用对应；不再自动丢弃上游差异                                      |
| HTTP/WS、队列与恢复   | native/headless-server                                  | 保留服务端权威状态，逐步分离请求处理与播放生命周期                                |
| 在线服务              | native/qqkg-api、native/streaming-api、外部 ncm-api-rs  | 将上游 Electron 在线业务修复映射到 Rust 实现                                      |
| Diretta FFI           | native/diretta-sys                                      | 固定 SDK/CPU 特性并单独做真机验证                                                 |
| 安装、打包、服务      | scripts                                                 | 建立独立 Headless 构建与 CI                                                       |
| 历史补丁              | tools/patch_alsa_dsd*.py 等                             | 确认最终效果和引用后归档，不能重放历史补丁作为构建步骤                            |

`.dbg` 和检查中出现的运行数据库未由 Git 跟踪；不应描述成误提交文件或直接删除。

## 上游同步规则

`bash scripts/sync-upstream.sh [remote] [branch]` 现在仅 fetch 并输出 SHA、提交及路径清单，不再自动合并、解决冲突或提交。旧脚本的自动执行语义已取消。

1. 从已验证的产品提交建立独立同步 worktree，固定目标上游 SHA。
2. 保存上游提交清单，为每项记录：直接合并、映射移植、产品不适用及理由。
3. 在隔离 worktree 中执行 `git -c rerere.enabled=false -c rerere.autoupdate=false merge --no-commit --no-ff <固定的上游SHA>`。保留共同历史，禁用旧自动冲突重放。
4. 上游 `native/audio-engine/src/` 的已迁移实现必须对照 `native/audio-engine-core/src/`，逐项确认修复是否需要移植；不得整体使用 ours。
5. Electron 业务代码与 UI 的依赖一起审核，桌面专属能力按产品范围判定。
6. 检查冲突标记和差异，执行 Web 类型检查/测试、Headless 编译/API/WS 测试；音频变更增加相应真机验收。
7. 检查通过后创建同步提交；进入 dev 和部署须分别记录代码版本与验证结果。

当前 14 项上游提交的初步路由（尚未合并或判定全部适用性）：

| 提交组                    | 审核重点                                                                    |
| ------------------------- | --------------------------------------------------------------------------- |
| dc8eab1、7eff9c4、8539ffc | 搜索播放、弹窗层级、加载动画；保留 Web/Diretta 新增行为                     |
| 251b3fb                   | QQ/网易云请求与响应变化；映射 qqkg-api、ncm-api-rs 和前端调用               |
| abb5a98                   | PipeWire 采样率修复映射到 core；按启用的 feature 判断是否进入 Headless 路径 |
| d2ddbfc、311eae5          | MPRIS 多艺人、桌面缩略图；先检查共享类型和前端依赖                          |
| eb9bc28、24bdf79、d3ef67f | 安装与 CI；保留 Headless 构建目标                                           |
| 其余 4 个 merge 提交      | 结合父提交核对，不重复移植已处理的变更                                      |

## 后续批次与验收门

1. 可复现构建：固定外部依赖，明确 Web 构建入口、SDK 和 CPU 配置，增加 Headless CI；干净环境构建通过。
2. 前端边界：扩展已有 Client 的曲库/配置/服务接口；未知 API 不再默默返回空数组；新增接口有契约测试。
3. 生命周期：围绕 load/stop/seek/失败回收逐步提取服务；验证 HTTP→ALSA DSD、快速连续切歌、取消、加载失败。
4. 上游集成：按本文件规则实际评估这 14 项更新并完成回归。
5. 交付整理：归档历史脚本和设计记录，提供当前唯一维护入口；验证安装、重启、队列恢复。

所有批次保留现有功能；一批一提交。源码快照、编译通过、测试通过与真机通过分别记录，不互相替代。
