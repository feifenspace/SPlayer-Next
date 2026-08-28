# Windows 原生模块依赖冲突排查参考

> 适用范围：在非 Windows 环境（如 Linux / macOS）执行 `cargo check --workspace` / `cargo build --workspace` 时，针对 Windows-only 原生模块（`taskbar-lyric`、`taskbar-thumbnail`）报出的 `windows-*` 编译错误。
> 结论先行：当前观察到的错误更符合目标平台不匹配，而不是依赖缺失或锁文件版本冲突。Windows 上是否复现，需要按本文第 4 节实际验证，不要仅凭 Linux 的报错修改依赖版本。

## 1. 现象

在 Linux/macOS 上对整个 workspace 做编译检查：

```bash
cargo check --workspace --all-targets
# 或 cargo build --workspace
```

会在编译 `windows-future` 时失败，错误类似：

```text
error[E0425]: cannot find function `marshaler` in module `windows_core::imp`
   --> .../windows-future-0.3.2/src/bindings.rs:752:470

error[E0425]: cannot find type `IMarshal` in module `windows_core::imp`
   --> .../windows-future-0.3.2/src/bindings.rs:927:377

error[E0425]: cannot find function `submit` in crate `windows_threading`
   --> .../windows-future-0.3.2/src/async_spawn.rs:264:28
```

使用依赖树定位来源：

```bash
cargo tree -i windows-future --workspace
```

当前锁文件中的关系是：

```text
windows-future v0.3.2
└── windows v0.62.2
    ├── taskbar-lyric v0.1.0 (.../native/taskbar-lyric)
    └── taskbar-thumbnail v0.1.0 (.../native/taskbar-thumbnail)
```

## 2. 根因分析

### 2.1 冲突链路

```text
taskbar-lyric / taskbar-thumbnail   (Windows-only 模块)
  └─ dependencies.windows = "0.62"
       └─ windows 0.62.2
            ├─ windows-core 0.62.2
            │    └─ imp 模块：IMarshal / marshaler 仅在 cfg(windows) 下存在
            ├─ windows-future 0.3.2
            │    └─ async_spawn.rs 引用 windows_threading::submit
            └─ windows-threading 0.2.1
                 └─ 整个 crate 标记为 #![cfg(windows)]，非 Windows 上无 submit
```

- `windows-core-0.62.2/src/lib.rs`：`pub mod imp;` 的 Windows 实现由 `#[cfg(windows)] include!("windows.rs")` 提供。非 Windows 下 `imp` 中没有 `IMarshal` / `marshaler`。
- `windows-threading-0.2.1/src/lib.rs`：整文件使用 `#![cfg(windows)]`，非 Windows 下不提供 `submit` 函数。
- `windows-future-0.3.2`：`async_spawn.rs` 与 `bindings.rs` 的编译条件主要受 `feature = "std"` 控制；`std` 默认开启时，在非 Windows 环境仍可能编译到引用 Windows 专有符号的代码。

### 2.2 为什么 workspace 检查会触发

- 根 [Cargo.toml](file:///home/songlian/SPlayer-Next-Headless/Cargo.toml) 的 `[workspace].members` 包含 `taskbar-lyric` 与 `taskbar-thumbnail`。
- 这两个 crate 的 [taskbar-lyric/src/lib.rs](file:///home/songlian/SPlayer-Next-Headless/native/taskbar-lyric/src/lib.rs) 与 [taskbar-thumbnail/src/lib.rs](file:///home/songlian/SPlayer-Next-Headless/native/taskbar-thumbnail/src/lib.rs) 未用 `#![cfg(windows)]` 守卫整个 `lib.rs`，而是在顶部直接引用 `windows::Win32::...`。
- 因此，非 Windows 上执行 `cargo check --workspace` 时会尝试编译这些 Windows-only crate，并连带解析 `windows` 依赖树，最终触发 §2.1 的符号缺失。
- `windows` 主 crate 本身（`windows-0.62.2/src/lib.rs`）使用 `#![cfg(windows)]`，但它的 `windows-core`、`windows-future`、`windows-threading` 等传递依赖仍可能在非 Windows 上被 Cargo 编译。

### 2.3 当前证据说明什么

- `cargo metadata --no-deps` 成功解析了 5 个 workspace 成员，依赖声明语法正确；
- `Cargo.lock` 已锁定 `windows 0.62.2`、`windows-core 0.62.2`、`windows-future 0.3.2`、`windows-threading 0.2.1`，目前没有证据表明存在版本解析冲突；
- [scripts/build-native.ts](file:///home/songlian/SPlayer-Next-Headless/scripts/build-native.ts) 按平台启用模块：`taskbar-lyric` 与 `taskbar-thumbnail` 仅在 `process.platform === "win32"` 时构建；
- 因此，Linux 上的失败应先视为 Windows-only 依赖被错误地放到非 Windows 检查路径中的信号。是否存在 Windows 本机问题，必须在 Windows target 上重新验证。

## 3. 影响范围速查

| 模块                | 平台                    | Windows 构建 | 非 Windows workspace 检查              |
| ------------------- | ----------------------- | ------------ | -------------------------------------- |
| `audio-engine`      | 跨平台                  | 应可构建     | 可单独检查；依赖 FFmpeg/rodio/FFT      |
| `audio-capture`     | Windows / Linux         | 应可构建     | Linux 走 PulseAudio，需 `libpulse-dev` |
| `media-ctrl`        | Windows / Linux / macOS | 应可构建     | 平台依赖按 `target_os` 选择            |
| `taskbar-lyric`     | **仅 Windows**          | 应可构建     | 可能触发 §1 的 `windows-future` 报错   |
| `taskbar-thumbnail` | **仅 Windows**          | 应可构建     | 可能触发 §1 的 `windows-future` 报错   |

## 4. Windows 上的正确验证步骤

在 Windows（推荐 Windows 10 1903+，见 [windows7.md](file:///home/songlian/SPlayer-Next-Headless/docs/troubleshooting/windows7.md)）执行：

```bash
# 前置：安装 Rust 工具链（rustup）并确认 MSVC target
rustc --version
cargo --version
rustup show active-toolchain
rustup target list --installed

# 方式一：项目统一构建原生模块（release）
pnpm install
pnpm build:native        # 通过 napi build；仅 win32 构建 taskbar-* 模块
# 调试版本
pnpm build:native --dev

# 方式二：单独构建某个 Windows-only 模块，便于定位
cd native/taskbar-lyric
napi build --release
cd ../taskbar-thumbnail
napi build --release
```

若 Windows 上仍报错，请按以下顺序排查：

1. **确认工具链 target**：检查是否安装 `x86_64-pc-windows-msvc`（或目标架构对应的 msvc target）。
2. **确认构建环境**：优先使用 MSVC 工具链和 Developer Command Prompt；确认 `rustc -vV` 的 host 与目标架构一致。
3. **确认锁文件未被手动改动**：在仓库根目录执行 `git status`，检查 `Cargo.lock` 是否有非预期变更。不要先升级 / 降级 `windows` 版本。
4. **逐模块隔离编译**：用上面的方式二分别构建，确定是 `taskbar-lyric` 还是 `taskbar-thumbnail` 触发问题。
5. **清理后重建**：在仓库根目录执行：

   ```bash
   cargo clean -p taskbar-lyric
   cargo clean -p taskbar-thumbnail
   pnpm build:native --dev
   ```

6. **保留完整环境信息**：记录 `rustc -vV`、`cargo --version`、`rustup show`、完整错误首个原因，以及 `Cargo.lock` 中所有 `windows-*` 版本。

## 5. 非 Windows 环境下的临时验证手段

如果只想验证跨平台模块是否健康，不要对整个 workspace 执行 `cargo check`：

```bash
# 只检查跨平台模块
cargo check -p audio-engine
cargo check -p audio-capture      # Linux 需要 libpulse-dev
cargo check -p media-ctrl

# 查看 windows 依赖来源
cargo tree -i windows-future --workspace
cargo tree -e normal -i windows-future --workspace
```

也可跳过原生构建，只做前端开发（见 [native.md](file:///home/songlian/SPlayer-Next-Headless/docs/native.md)）：

```bash
SKIP_NATIVE_BUILD=true pnpm dev
```

## 6. 常见误解澄清

- ❌ 「`Cargo.toml` 漏写了某个 `windows` 依赖」 → ✅ 目前依赖声明完整，`cargo metadata` 已验证。
- ❌ 「看到 `windows-future` 报错就立即升级 / 降级 `windows`」 → ✅ 先区分当前目标平台；Linux 上的该错误不能直接证明 Windows 版本冲突。
- ❌ 「在 Linux 上 `cargo check --workspace` 通过才算配置正确」 → ✅ Windows-only 模块应在 Windows target 上验证；非 Windows 环境应检查跨平台 crate，或使用交叉编译 target 做进一步验证。
- ❌ 「`Cargo.lock` 中 `windows-future 0.3.2` 与 `windows-core 0.62.2` 必然不匹配」 → ✅ 这两个版本由 `windows 0.62.2` 的传递依赖约束解析得到，当前没有证据表明它们之间存在 Cargo 版本解析冲突。

## 7. 相关文件索引

- 工作区配置：[Cargo.toml](file:///home/songlian/SPlayer-Next-Headless/Cargo.toml)
- 构建脚本（按平台启用模块）：[scripts/build-native.ts](file:///home/songlian/SPlayer-Next-Headless/scripts/build-native.ts)
- Windows-only 模块：
  - [native/taskbar-lyric/Cargo.toml](file:///home/songlian/SPlayer-Next-Headless/native/taskbar-lyric/Cargo.toml)
  - [native/taskbar-thumbnail/Cargo.toml](file:///home/songlian/SPlayer-Next-Headless/native/taskbar-thumbnail/Cargo.toml)
- 原生模块总览：[docs/native.md](file:///home/songlian/SPlayer-Next-Headless/docs/native.md)
- 依赖版本锁定：[Cargo.lock](file:///home/songlian/SPlayer-Next-Headless/Cargo.lock)（`windows` / `windows-core` / `windows-future` / `windows-threading` / `windows-collections`）
