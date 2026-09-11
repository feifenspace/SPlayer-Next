# Web / Headless 构建

本说明对应维护分支的构建整理，替代依赖开发者固定主目录的构建步骤。

## 依赖

- Node.js >= 22.19.0，pnpm 使用 package.json 中固定的版本。
- Rust 工具链及 C/C++ 编译环境；Linux 需要 ALSA、OpenSSL 开发包、pkg-config、clang、cmake。
- 网易云 API 由 Cargo 从 https://github.com/SPlayer-Dev/ncm-api-rs 获取，固定在 `133b65bfe482e41ebccf018870d3fce07bf58eb3`；不再需要 `/home/songlian/ncm-api-rs`。
- 首次获取依赖需要网络。之后可以在依赖已缓存的环境使用 `--offline`。所有正式构建使用 `--locked`，不自动更新锁文件。

## Web 控制台

```bash
pnpm install --frozen-lockfile --ignore-scripts
pnpm typecheck:web
pnpm test:web
pnpm build:web
```

`--ignore-scripts` 用于此 Web/Headless 流程，跳过桌面安装钩子；不作为 Electron 开发安装方式。Web 配置复用上游 renderer 的插件、别名和版本常量，仅以根 index.html 为入口，产物写入 `out/renderer`。生产 Web 构建不改写共享自动生成类型文件。

## 无 SDK 开发与 CI

未设置 `DIRETTA_SDK_DIR` / `DIRETTA_SDK_ROOT` 时构建 Diretta 占位实现，可用于 Headless 编译和协议测试；不具备真实 Diretta 推流能力。

```bash
cargo check --locked -p headless-server
cargo test --locked -p headless-server --lib --test library_db_test --test playlist_config_test --test static_hosting_test
```

`.github/workflows/headless.yml` 提供此流程，并执行 Web 检查与构建。GitHub 托管机器没有安装私有 SDK，因此 CI 不能替代 Diretta 真机测试。

## 使用真实 Diretta SDK

```bash
DIRETTA_SDK_DIR=/path/to/DirettaHostSDK_150 DIRETTA_ARCH=v2 cargo build --locked --release -p headless-server
```

SDK 路径必须包含 Host 和 lib 目录以及所选架构的静态库。显式指定路径不正确时构建失败，避免以为已包含 SDK 却实际生成占位版本。直接 cargo 构建不再自动搜索开发者主目录；打包脚本仍支持用户主目录及 /opt 下的发现流程。

```bash
bash scripts/package-linux-headless.sh --sdk-dir /path/to/DirettaHostSDK_150 --arch v2
```

发布包默认输出到项目 `dist`，可通过 `--output-dir` 修改。打包调用 `pnpm build:web` 和锁定依赖的 Cargo 构建。打包不再自动删除 `target/debug`，保留调试与测试缓存。

Web 静态资源仍随发布目录的 web 子目录交付，服务端可从磁盘读取；本次没有改变源码内的占位资源内嵌策略，也未执行完整 release 打包或部署。

## 本批验证记录（2026-09-11）

- 在独立工作区使用锁文件离线安装 JS 依赖成功（跳过桌面安装脚本）。
- `pnpm build:web`、`pnpm typecheck:web`、74 项 Web 测试通过。
- 独立 TypeScript 检查 Vite 配置通过；ESLint 与 shell 语法检查通过。
- 构建后共享声明文件无差异，未生成 out/main、out/preload 或桌面歌词入口。
- 固定 Git 来源的 ncm-api-rs 下载和锁文件更新成功，只新增该包来源记录。
- 无 SDK 编译通过；真实 SDK 150/v2 的 `cargo check --locked --offline` 通过。
- Rust 服务端测试：17 项库单元测试、5 项曲库测试、3 项歌单/配置测试通过；静态托管测试首次并行运行因共用默认数据库发生锁冲突，改为每项使用独立临时数据库后 4 项全部通过（共覆盖 29 项）。
- 错误 SDK 路径检查按预期失败，明确报告 `DirettaHostSDK Host/ or lib/ missing`，未静默回退到占位版本。
- 保留基线已有 3 条 Rust 编译警告、Web 大 chunk 提示和部分测试网络回退日志。

这些结果不代表硬件播放或 GitHub 远端 CI 已通过；原 dev 和运行服务维持原状。
