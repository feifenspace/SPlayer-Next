# Headless Web 模式前端功能匹配度修复方案

> 状态：待评审（2026-09-05 完成两轮复核：第一轮逐项代码复核修正 4 处；第二轮能力可实现性复核推翻约 15 项「不可用」判定并重构 §5，见 §8）
> 范围：`src/`（渲染进程）为主，`native/headless-server/`（Rust）配套改动
> 原则：**桌面端零改动**——所有修复在 Electron 桌面环境必须是无操作（polyfill 仅在 `!window.electron` 时注入；设置项 `visible` 谓词在桌面端恒为 true）

---

## 1. 背景与问题定性

Web 模式的适配分三层：

1. `src/services/client/index.ts:13` 以 `window.electron.ipcRenderer` 判断环境，Web 下使用 `HttpPlayerClient`（HTTP + WebSocket 对接 Rust Axum 服务）；
2. `src/main.ts:1` 首行导入 `src/services/client/webPolyfill.ts`，为浏览器伪造完整 `window.api`：`player / config / library / playlist / stats(部分) / lyrics / comments / streaming / diretta / apis` 为真实 HTTP 实现，其余走 `createSafeProxy` 空实现；
3. 无独立 Web 构建，直接复用 `out/renderer` 产物由服务端静态托管。

排查结论：核心播放链路匹配良好，但 **UI 层没有任何 headless 裁剪**（仅 `SideBar.vue:186` 一处按 `download.enabled` 隐藏入口），桌面专属功能全部照常渲染；另有 4 个 Web 模式下会**崩溃 / 卡死 / 数据错误**的实际 Bug。

### 修复项总表

| 编号 | 级别 | 问题 | 类型 |
|---|---|---|---|
| FIX-1 | P0 | 「关于」设置页打开即崩溃 | Bug |
| FIX-2 | P0 | 听歌识曲误判支持并永久卡在「识别中」 | Bug |
| FIX-3 | P0 | 应用内快捷键被空绑定覆盖而全部失效 | Bug |
| FIX-4 | P0 | 首页「本周时长」显示 NaN | Bug |
| FIX-5 | P0 | 手动检查更新永久卡在「检查中」 | Bug |
| FIX-6 | P0 | 歌曲缓存 lookup 的 stub 对象可能被当作音源 URL | 隐患 |
| FIX-7 | P0 | 按 UA 判定平台，Windows 浏览器误开 Win 专属设置区 | Bug |
| UI-1 | P1 | 统一 Web 模式判断工具 + 布局层桌面控件裁剪 | 裁剪 |
| UI-2 | P1 | 设置页按 Web 模式隐藏无效分类/设置项 | 裁剪 |
| UI-3 | P1 | 右键菜单 / 页面入口裁剪（下载、标签编辑、云盘上传、识曲等） | 裁剪 |
| ENH-1 | P2 | 播放统计页数据：服务端聚合端点或前端聚合 | 增强 |
| ENH-2 | P2 | 本地曲目外置歌词（服务端探测同名 .lrc） | 增强 |
| ENH-3 | P2 | 封面取色走服务端图片代理规避 CORS | 增强 |
| ENH-4 | P2 | `OutputDeviceSelector` 孤儿组件接线 + 服务端设备枚举 | 增强 |
| ENH-5 | P2 | Web 更新检查入口语义化（跳转 Releases） | 增强 |
| ENH-6 | P2 | 背景图 dataURL 迁移至 IndexedDB 规避 localStorage 配额 | 增强 |
| SRV-1 | ✅ 已落地 | FFT 频谱 WS 订阅转发（`0e9d96d` 已实现，含 seeked 事件） | 能力扩展 |
| SRV-2 | P2↑ | EQ/变速/变调/淡入淡出/响度均衡端点（Player API 已逐一确认） | 能力扩展 |
| SRV-5 | P2 | Last.fm / ListenBrainz Scrobble（纯 HTTP 移植） | 能力扩展 |
| SRV-6 | P2 | 标签编辑与本地曲目删除（服务端文件操作） | 能力扩展 |
| SRV-7 | P2 | 服务端下载进曲库 | 能力扩展 |
| SRV-8 | P2 | 云盘上传与文件拖放导入 | 能力扩展 |
| SRV-9 | P2 | MediaSession 媒体控制 / 媒体键（纯浏览器端） | 能力扩展 |
| SRV-3 | P3 | 服务端识别端点 | 可选 |
| SRV-4 | P3 | 流媒体凭证服务端存储（消除明文 IndexedDB） | 可选 |
| SRV-10 | P3 | PiP 歌词小窗 + URL 深链（浏览器等价形态） | 可选 |

---

## 2. P0：Bug 修复（全部只动 `webPolyfill.ts` 与个别调用点，桌面端零影响）

### FIX-1 「关于」页崩溃

- **现象**：Web 模式打开 设置 → 关于，抛 `TypeError: Cannot read properties of undefined (reading 'process')`。
- **根因**：`src/components/settings/custom/AboutSettings.vue:29` 在 setup 顶层直接访问桌面对象，且这是全库唯一未加防护的 `window.electron` 直引：

```ts
// AboutSettings.vue:29（现状）
const versions = window.electron.process.versions;
```

`versions` 不止用于渲染：`envItems` computed（:55-71）把 `versions.electron / versions.chrome / versions.node / versions.v8` 四行拼进环境信息列表，同时是「复制环境信息」（`handleCopyEnvInfo`，:73-76）的数据源。

- **修复**：

```ts
// AboutSettings.vue
const isWeb = !isElectron(); // 从 "@/services/client" 导入
const versions = window.electron?.process?.versions;
```

`envItems` 中四个桌面运行时条目改为条件展开：

```ts
...(versions
  ? [
      { label: "Electron", value: versions.electron },
      { label: "Chromium", value: versions.chrome },
      { label: "Node.js", value: versions.node },
      { label: "V8", value: versions.v8 },
    ]
  : []),
```

Web 模式下环境信息卡片由「Web 模式」说明替代（可选增强：从 `GET /api/status` 读取服务端版本展示）。同页「打开日志目录」（`system.openLogsDir` stub）与「检查更新」按钮在 Web 下随 UI-2 一并隐藏。

- **涉及文件**：`src/components/settings/custom/AboutSettings.vue`
- **验证**：Web 模式打开「关于」分类不再报错，桌面端环境信息表不变。

---

### FIX-2 听歌识曲卡死

- **现象**：Web 模式点击识曲，UI 永久停留在「识别中」，无法完成也无法恢复。
- **根因**（两处叠加）：
  1. `webPolyfill.ts:695` `recognition: createSafeProxy("recognition")` 无任何显式方法。`isSupported` 不匹配 safe proxy 的 `get/list/fetch` 前缀规则，落入通用分支返回 `{ success: false, ... }` **对象**（truthy）；
  2. `src/composables/useRecognitionSession.ts:165-168` 将该对象直接赋给 `supported.value` → `start()`（:137-145）误判「原生采集可用」走 `window.api.recognition.start()` stub，永无事件回推。
  讽刺的是浏览器麦克风路径（`captureInRenderer`，`getUserMedia` + AudioWorklet，`src/services/recognition/microphoneCapture.ts`）本身是纯浏览器 API，技术上 Web 可用——但因上述误判永远走不到；即使走到，`submitPcm`（:113）返回 stub 对象后同样无人推进状态。

- **修复**（P0 先做「诚实降级」，服务端化识别放 SRV-3）：

  1. polyfill 显式声明能力为 false：

```ts
// webPolyfill.ts
recognition: createSafeProxy("recognition", {
  isSupported: async () => false,
}),
```

  2. `useRecognitionSession.ts` 的 `captureInRenderer` 对 `submitPcm` 结果做兜底，消灭「卡 capturing」这一类问题（对桌面端同样是加固）。已核实桌面端 `recognition:submitPcm` 返回 `{ success: true }`（`electron/main/ipc/recognition.ts:28-30`），以下检查在桌面端不会误报：

```ts
// useRecognitionSession.ts:113 附近
const res = await window.api.recognition.submitPcm(pcm);
if (!res || res.ok === false || res.success === false) {
  error.value = { code: "capture-failed", message: "识别服务不可用" };
  phase.value = "error";
  resumePlayback();
}
```

  3. Web 模式隐藏识曲入口（见 UI-3），避免用户进入一条必然失败的路径。
  4. `stop()` 中 `window.api.recognition.cancel()` 返回 stub 无害，不动。

- **涉及文件**：`src/services/client/webPolyfill.ts`、`src/composables/useRecognitionSession.ts`、`src/components/layout/NavSearch.vue`（入口隐藏）
- **验证**：Web 模式识曲入口不可见；桌面端识曲行为不变（`isSupported` 走 preload 真实实现）。

---

### FIX-3 应用内快捷键失效

- **现象**：Web 模式所有快捷键（含纯应用内的播放/暂停、音量、切曲等）无效；设置页改动无法保存。
- **根因**：`webPolyfill.ts:138-147` `hotkey.getAll` 恒返回 `{ bindings: {}, globalEnabled: false }`，`src/stores/hotkey.ts:21-34` init 时 `applyConfig` 用它**覆盖**默认键表；而 `src/core/hotkey/manager.ts` 的 keydown 编译只依赖 `bindings[id].inApp`（`recompile()` 不看 `globalEnabled`）——即只要绑定表在，应用内快捷键在浏览器完全可工作。设置页 `hotkey.set/reset` 也返回空配置，改动即被清空。
- **修复**：polyfill 提供基于 **localStorage 持久化**的完整 hotkey 实现，默认值取 `shared/defaults/hotkeys.ts:151` 的 `defaultHotkeyConfig`（仅 `globalEnabled` 置 false，global 作用域在 Web 不存在）：

```ts
// webPolyfill.ts
import { defaultHotkeyConfig } from "@shared/defaults/hotkeys";
import type { HotkeyConfig, HotkeyActionId, HotkeyBinding } from "@shared/types/hotkey";

const HOTKEY_STORAGE_KEY = "splayer/web-hotkey-config";

const readHotkeyConfig = (): HotkeyConfig => {
  try {
    const raw = localStorage.getItem(HOTKEY_STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as HotkeyConfig;
      if (parsed && typeof parsed === "object" && parsed.bindings) return parsed;
    }
  } catch { /* 损坏配置按默认处理 */ }
  return { ...defaultHotkeyConfig, globalEnabled: false };
};

const writeHotkeyConfig = (cfg: HotkeyConfig): HotkeyConfig => {
  localStorage.setItem(HOTKEY_STORAGE_KEY, JSON.stringify(cfg));
  return cfg;
};

// 替换现有 hotkey stub
hotkey: {
  getAll: async () => readHotkeyConfig(),
  getConflicts: async () => [],
  onConflicts: () => () => {},
  onTrigger: () => () => {}, // global 触发在 Web 不存在；inApp 由 manager keydown 直接分发
  set: async (id: HotkeyActionId, binding: HotkeyBinding) => {
    const cfg = readHotkeyConfig();
    return writeHotkeyConfig({ ...cfg, bindings: { ...cfg.bindings, [id]: binding } });
  },
  reset: async (id?: HotkeyActionId) => {
    const cfg = readHotkeyConfig();
    if (id) {
      const bindings = { ...cfg.bindings };
      if (defaultHotkeyConfig.bindings[id]) bindings[id] = defaultHotkeyConfig.bindings[id];
      else delete bindings[id];
      return writeHotkeyConfig({ ...cfg, bindings });
    }
    return writeHotkeyConfig({ ...defaultHotkeyConfig, globalEnabled: false });
  },
  setGlobalEnabled: async (enabled: boolean) => {
    return writeHotkeyConfig({ ...readHotkeyConfig(), globalEnabled: enabled });
  },
  probe: async () => true, // Web 无 OS 注册层，仅 inApp 生效，探测恒可注册
},
```

注意：返回结构必须是完整的 `HotkeyConfig`（`{ bindings, globalEnabled }`），`applyConfig` 直接整体覆盖，不可返回部分对象。

- **涉及文件**：`src/services/client/webPolyfill.ts`
- **验证**：Web 模式刷新后按空格暂停/播放、`Ctrl+Right` 切曲等默认 inApp 快捷键生效；设置页改绑后刷新仍生效；桌面端走 preload，不受影响。补充 `client.spec.ts` 用例：`getAll` 默认返回 `defaultHotkeyConfig` 形状、`set` 后 `getAll` 能读回。

---

### FIX-4 首页统计 NaN

- **现象**：Web 模式首页「本周时长」显示 `NaN`。
- **根因**：`window.api.stats.getStatsSummary` 未在 polyfill 的 stats defaults 中定义（`webPolyfill.ts:329-349` 只有 getLibraryStats/recordPlay/getPlayHistoryDaily/getPlayHistoryHourly），落入 safe proxy 的 `get` 前缀规则返回 **`[]`**（truthy 数组）；`src/composables/home/useHomeHeader.ts:135` 将其赋给 `stats`，:69-72 计算 `(data.weekListenedMs / 3600000).toFixed(1)` 得 NaN。
- **修复**：polyfill 显式补齐 `PlayStatsSummary` 全零对象（形状见 `shared/types/stats.ts:28-45`，共 8 个数值字段，已逐字段核对）：

```ts
// webPolyfill.ts stats defaults 内追加
getStatsSummary: async () => ({
  todayListenedMs: 0,
  weekListenedMs: 0,
  lastWeekListenedMs: 0,
  totalListenedMs: 0,
  weekPlayCount: 0,
  totalPlayCount: 0,
  weekFavoriteAdds: 0,
  streakDays: 0,
}),
getTopTracks: async (_limit: number) => [],
getTopAlbums: async (_limit: number) => [],
getTopArtists: async (_limit: number) => [],
```

签名对齐 `shared/types/stats.ts:112-124` 的 `StatsApi`（`getTopTracks/Albums/Artists(limit)`）。

**复核补充发现**：polyfill 现有 `getPlayHistoryDaily`（`webPolyfill.ts:344-347`）把 `/api/v1/stats/history` 的**原始播放记录**直接返回，而唯一消费方 `Stats.vue:28` 期望的是按天聚合的 `DailyPlayStats[]`（`{ day: "YYYY-MM-DD", playCount }`，`shared/types/stats.ts:72-77`）——形状不匹配，热力图拿到的是错误形状数据而非空。P0 阶段先把它规范化为返回 `[]`（优雅空态），真实聚合由 ENH-1 解决：

```ts
// webPolyfill.ts —— 替换现有 getPlayHistoryDaily 映射
getPlayHistoryDaily: async (_days: number) => [], // 聚合口径见 ENH-1
```

兜底改法（二选一，推荐 polyfill 方案）：`useHomeHeader.ts:135` 处校验返回值非数组。

- **涉及文件**：`src/services/client/webPolyfill.ts`
- **验证**：Web 模式首页三项统计显示 0 而非 NaN；桌面端不变。

---

### FIX-5 手动检查更新卡死

- **现象**：Web 模式设置 → 检查更新，状态永久「检查中」。
- **根因**：桌面端 `phase` 由主进程经单一事件通道推送复位。`src/stores/update.ts:57` 订阅 `window.api.update.onEvent(handleEvent)`，事件为 `UpdateEvent` 联合类型（`{ type: "checking" | "available" | "notAvailable" | "progress" | "downloaded" | "error", manual?, ... }`，见 `shared/types/update.ts:19-26`）；`notAvailable` 分支复位 `phase="upToDate"`（update.ts:36-39）。而 polyfill 的 `update` stub（`webPolyfill.ts:441-449`）**根本没有 `onEvent` 方法**——它定义的 `onChecking/onUpdateAvailable/onUpdateNotAvailable/...` 是 store 从不订阅的死方法（preload 的真实形状见 `electron/preload/index.ts:712-720`，就是单一 `onEvent`）。因此 `checkManually`（update.ts:63-66）置 `phase="checking"` 后永无复位。
- **修复**：polyfill 按真实协议模拟——单一 `onEvent` 通道 + `check` 时派发 `checking → notAvailable` 事件序列：

```ts
// webPolyfill.ts（替换 update stub）
import type { UpdateEvent } from "@shared/types/update";

const updateEventListeners = new Set<(event: UpdateEvent) => void>();
const fireUpdateEvent = (event: UpdateEvent): void => {
  for (const cb of updateEventListeners) cb(event);
};

update: createSafeProxy("update", {
  check: async (manual: boolean) => {
    fireUpdateEvent({ type: "checking" });
    fireUpdateEvent({ type: "notAvailable", manual });
    return { hasUpdate: false };
  },
  onEvent: (cb: (event: UpdateEvent) => void) => {
    updateEventListeners.add(cb);
    return () => updateEventListeners.delete(cb);
  },
}),
```

要点：事件必须走 `onEvent` 通道（store 唯一订阅入口）；`manual` 透传使手动检查能弹「已是最新」toast（update.ts:38）、启动检查静默。该方案不改 store；ENH-5 会把 Web 模式的更新入口替换为跳转 GitHub Releases，本修复作为底层兜底保留。
- **涉及文件**：`src/services/client/webPolyfill.ts`
- **验证**：Web 模式启动后 `phase` 经 checking→upToDate 落定；手动检查弹出「已是最新」toast，不卡 checking；桌面端事件流不变。

---

### FIX-6 歌曲缓存 lookup 污染音源

- **现象**（条件触发）：服务端配置 `system.cache.songCache.enabled === true` 时，Web 模式解析音源会把 safe proxy 的 stub 对象当作缓存命中结果传给 `player.load`，导致加载失败。
- **根因**：`src/services/audioSource.ts:292-293` `const cached = await window.api.cache.song.lookup(key); if (cached) return { source: cached, ... }`——polyfill 的 `cache` stub（`webPolyfill.ts:395-400`）未定义 `song.lookup`，safe proxy 返回 truthy 对象。桌面端 preload 的真实签名是 `lookup: (cacheKey) => Promise<string | null>`（`electron/preload/index.ts:528-529`），即「未命中 = null」。
- **修复**：polyfill 显式声明与桌面端一致的语义（未命中 = `null`）：

```ts
// webPolyfill.ts cache defaults 内追加
song: {
  lookup: async () => null,
  fetch: async () => ({ success: false }),
},
```

- **涉及文件**：`src/services/client/webPolyfill.ts`
- **验证**：开启 songCache 配置后 Web 模式播放在线歌曲正常回退到在线解析。

---

### FIX-7 平台误判

- **现象**：Windows 浏览器访问时 `platform` 被判为 `win32`，误展示任务栏歌词等 Win 专属设置区（`src/settings/categories/externalLyric.ts:398`、`general.ts:59` 按 `navigator.platform`/platform 判断）。
- **根因**：`webPolyfill.ts:33-40` `detectPlatform()` 按浏览器 UA 返回 win32/darwin/linux。Web 模式下这些平台分支控制的是**服务端**行为（任务栏歌词属桌面主进程），浏览器客户端的 OS 与之无关。
- **修复**：headless 服务端当前仅支持 Linux，直接固定返回：

```ts
// webPolyfill.ts
const detectPlatform = (): NodeJS.Platform => "linux";
```

（`utils/config.ts:9` 的 `platform ?? "linux"` 默认值亦与此一致。）若未来服务端支持 macOS，可从 `/api/status` 读取服务端 OS 下发。
- **涉及文件**：`src/services/client/webPolyfill.ts`
- **验证**：Windows 浏览器访问不再出现任务栏歌词设置区；`isMac` 分支（桌面歌词分类过滤）在 Web 下行为与 Linux 桌面端一致。

---

## 3. P1：UI 裁剪（让「展示但无效」变成「不展示」）

### UI-0 前置：统一 Web 判断工具

`isElectron()` 已存在于 `src/services/client/index.ts:13`，但组件层零使用。新增一个语义化导出，供模板与 schema 谓词复用：

```ts
// src/services/client/index.ts 追加导出
export const isWebMode = (): boolean => !isElectron();
```

### UI-1 布局层裁剪

| 位置 | 现状 | 改法 |
|---|---|---|
| 窗口控制按钮（最小化/最大化/关闭） | `src/components/layout/WindowControls.vue:23` 组件根元素 `v-if="isBorderless"`，默认 true，Web 照常渲染（挂载点：`NavHeader.vue:110-111`、`FullPlayer/index.vue:255`） | **单点修复**：组件根 `v-if` 追加 `&& !isWebMode()`，一处覆盖全部挂载点，不必逐挂载点修改 |
| 桌面歌词按钮 | `Toolbar.vue:119-128`（`toggleDesktopLyric` 定义在 :53-54）常驻 | `v-if="!isWebMode()"` |
| 识曲入口 | `NavSearch.vue:299,304`（`recognitionOpen` 触发） | `v-if="!isWebMode()"`（配合 FIX-2） |
| 侧边栏 `/download` 入口 | 已按 `download.enabled` 隐藏（默认 false） | 不动；但需在 UI-2 隐藏下载设置分类，防止用户在 Web 打开开关后入口复活 |

### UI-2 设置页裁剪

**同步友好性约束（本节核心要求）**：`src/settings/categories/*.ts` 与各分类文件是**上游活跃开发的共享文件**（定期 `git merge origin/dev`），禁止在几十个 Item 上逐个散布 `visible: desktopOnly` 谓词——那会制造大量冲突点。隐藏清单必须**中心化**，收敛到 fork 独有文件：

```ts
// src/settings/webHidden.ts（fork 独有，上游无此文件）
/**
 * Web 模式隐藏的设置分类/区块/条目 key 清单。
 * 粒度："分类id" | "分类id.区块id" | "分类id.区块id.条目id"，前缀匹配。
 * 内容来源：下方「各分类处置」表；桌面端不消费此清单，零行为影响。
 */
export const WEB_HIDDEN_KEYS: ReadonlySet<string> = new Set([
  "externalLyric",
  "plugins",
  "aiIntegration",
  "download",
  // …其余条目 key 见处置表
]);
```

消费端只在两处接入（均为上游共享文件的**单点小改**）：
- `SettingsContent.vue` / `useSettingModel` 渲染遍历处：跳过命中清单的 分类/区块/条目；
- `SettingsSearch.vue:29-33`：现有循环已在逐级检查 `sec.visible`/`item.visible`，同处追加清单判断。

**类型变更**：`SettingCategory`（`src/types/settings-schema.ts:108-115`）目前没有 `visible` 字段（仅 Section/Item 有，:65/:104），补齐一个可选字段：

```ts
export interface SettingCategory {
  id: string;
  icon: Component;
  sections?: SettingSection[];
  component?: Component;
  /** 条件隐藏整个分类（配合 webHidden 清单使用） */
  visible?: () => boolean;
}
```

`SettingsSearch.vue:29-33` 已在逐级检查 `sec.visible`/`item.visible`，分类级在同一循环开头加 `if (cat.visible && !cat.visible()) continue;` 即可——schema.ts 与消费方合计 3 处单点改动。

**辅助谓词**（放在 `src/settings/predicates.ts`，fork 独有）：

```ts
import { isWebMode } from "@/services/client";

/** 仅桌面端可见（Web 模式隐藏） */
export const desktopOnly = (): boolean => !isWebMode();
```

**各分类处置**：

| 分类 | Web 模式处置 | 理由 |
|---|---|---|
| `externalLyric` 外置歌词 | 分类级 `visible: desktopOnly` | 三个歌词窗口均为桌面主进程窗口，40+ 项全部无效 |
| `plugins` 插件 | 分类级隐藏 | 插件 host 在主进程 worker；列表恒空、无法安装 |
| `aiIntegration` AI 集成 | 分类级隐藏 | MCP/AI 模型全部 stub |
| `download` 下载 | 分类级隐藏 | 无下载引擎；防止打开总开关复活侧边栏入口 |
| `hotkeys` 快捷键 | **保留**（FIX-3 后 inApp 生效），分类内「全局快捷键」开关所在 Section 加 `visible: desktopOnly` | global 作用域 Web 不存在 |
| `services` 网络与服务 | 保留分类；隐藏 Item/Section：系统媒体控制、Discord、Last.fm、ListenBrainz、External API 状态卡、代理测试（`testNetworkProxy` 恒 true 假成功，代理项可保留但隐藏测试按钮） | 均为主进程能力 |
| `general` 通用 | 隐藏「系统窗口」组（记忆窗口/无边框/任务栏进度/缩略图/orpheus 协议/关闭行为）与「更新」组；保留语言、备份/重置 | 无窗口对象；更新走 ENH-5 |
| `player` 播放 | 隐藏：淡入淡出、响度均衡、均衡器、常规输出设备、切换暂停（`httpClient.ts:743-797` 全为空实现）；保留自动播放/音质/预载/在线源/频谱 | DSP 能力 SRV-2 落地后再放开 |
| `localCache` 本地缓存 | 隐藏歌曲缓存开关与容量项（写 config 无效且有 FIX-6 之外的实际下载链路缺失）；文件/数据库缓存管理器隐藏 | `cache` stub 统计恒 0、清理假成功 |
| `other` 其它 | 隐藏酷狗「登录版本」项；保留 Cookie 登录（真实可用）与渲染层 preset 项 | — |
| `about` 关于 | 隐藏「检查更新」「打开日志目录」；环境信息表随 FIX-1 隐藏 | — |
| `appearance` / `lyric` / `mediaSource`(streaming) | **完整保留** | 纯渲染层或真实 HTTP |

> 实施说明：下表的隐藏动作全部通过 `webHidden.ts` 清单（分类/区块/条目 key）实现，不直接修改 `categories/*.ts` 上游文件；各分类文件随上游演进时零冲突。

### UI-3 动作入口裁剪

| 入口 | 现状 | 改法 |
|---|---|---|
| 右键菜单「下载」 | `useTrackMenu.ts:117-121` 受 `download.enabled` 控制，开启后点击静默失败且提示误导性「已在队列」（`useDownload.ts:74-78`） | 菜单项谓词追加 `&& !isWebMode()` |
| 右键菜单「在文件夹中显示」 | `useTrackMenu.ts:239` stub 无操作 | Web 下不渲染该项 |
| 右键菜单「插件菜单」 | `useTrackMenu.ts:211` 点击报 not supported toast | Web 下不渲染插件分组 |
| 标签编辑入口 | `TagEditorDialog.vue:86,100` `readTags/pickCoverImage` stub，编辑必失败 | 本地曲目的「编辑标签」菜单项 Web 下隐藏（在线曲目本就不提供） |
| 云盘「上传」按钮 | `cloudUpload.ts:140` `pickAndEnqueue` stub，入队即 error | Web 下隐藏上传按钮；云盘浏览/删除保留（真实代理可用） |
| 登录弹窗「打开网页版登录」 | `LoginDialog.vue:49` `openLoginWeb` stub `{ok:false}` | Web 下隐藏该按钮；扫码/Cookie 登录保留（真实可用） |
| 设置导入/导出 | 浏览器 Blob/file input 实现，真实可用 | 不动 |

---

## 4. P2：降级功能增强

### ENH-1 播放统计页数据

- **现状**：服务端 `/api/v1/stats/summary` 返回的是**音乐库统计**（`get_library_stats`，`routes.rs:2220-2228`），不是桌面端 `PlayStatsSummary`；`getTopTracks/Albums/Artists`、`getPlayHistoryHourly` 无服务端对应；polyfill 的 `getPlayHistoryDaily` 把 `/api/v1/stats/history` 原始记录直接映射给期望 `DailyPlayStats[]`（按天聚合）的消费方，形状不匹配。
- **方案 A（推荐）——服务端聚合端点**，对齐 `electron/main/database/playStats.ts` 的 SQL 语义：

  ```
  GET /api/v1/stats/summary/play   → PlayStatsSummary（today/week/lastWeek/total/streak）
  GET /api/v1/stats/top?kind=tracks|albums|artists&limit=N&days=D → TopTrack[]/TopAlbum[]/TopArtist[]
  GET /api/v1/stats/hourly?days=D  → 按小时累计播放
  GET /api/v1/stats/daily?days=D   → DailyPlayStats[]（按天聚合，修正现有映射）
  ```

  播放记录已在写入（`POST /api/v1/stats/record`），只需在 `native/headless-server/src/db.rs` 增加聚合查询；polyfill 中将 stats 各方法映射到新端点。
- **方案 B（不动 Rust）——前端聚合**：polyfill 拉取 `/api/v1/stats/history`（原始记录含 track 信息与时间戳）后在 `webPolyfill.ts` 内做按天/按小时/Top-N 聚合。缺点是记录量大时浏览器开销高，且「本周/连续天数」的口径要与桌面端对齐。
- **验收**：Stats 页 Top10 曲目/专辑/歌手、热力图、首页三项统计均有数据；口径与桌面端一致。

### ENH-2 本地曲目外置歌词

- **现状**：`HttpPlayerClient.load` 硬编码 `externalLyrics: []`（`httpClient.ts:547,625`），`readLyricFile` 恒失败（:755-760）→ 本地曲目永远拿不到同名 `.lrc` 外置歌词（`shared/types/player.ts:148` 定义 `externalLyrics: { format; path }[]`）。
- **改法**：
  1. 服务端 `load_handler`（`routes.rs:719` 附近的 meta 处理处）探测音源同名歌词文件（`<stem>.lrc/.srt`，复用音乐库扫描时的目录信息），返回 `external_lyrics: [{ format, path }]`；
  2. `httpClient.load` 映射 `payload.external_lyrics` → `detail.externalLyrics`；
  3. `readLyricFile` 改走已有免鉴权端点 `GET /api/v1/lyrics/file?path=`（`routes.rs:293`），替换现在的恒失败实现。
- **验收**：本地曲目存在同名 .lrc 时，Web 播放即显示外置歌词；无外置歌词时回退在线匹配（现状行为）。

### ENH-3 封面取色规避 CORS

- **现状**：Web 模式 `system.fetchRemoteBytes` 为浏览器直接 fetch（`webPolyfill.ts:91-100`），跨域封面取色失败时主题色退化。
- **改法**：失败时回退服务端图片代理（已有，`routes.rs` `image_proxy_handler`，`/api/proxy/image`）：

```ts
fetchRemoteBytes: async (url: string) => {
  const attempts = [url, `${playerClientUrl()}/api/proxy/image?url=${encodeURIComponent(url)}`];
  for (const target of attempts) {
    try {
      const res = await fetch(target);
      if (!res.ok) continue;
      return { success: true, data: new Uint8Array(await res.arrayBuffer()) };
    } catch { /* 尝试下一个通道 */ }
  }
  return { success: false, data: null };
},
```

- **验收**：流媒体（Jellyfin/Emby/Subsonic）与网易外链封面在 CORS 拒绝场景下仍能取色成功。

### ENH-4 输出设备选择器接线

- **现状**：`src/web/OutputDeviceSelector.vue`（专为 headless 编写，含 ALSA 硬件 PCM / Diretta 过滤逻辑）在 `src/` 内零引用；且 `httpClient.getOutputDevices` 恒返回 `[]`，接了也没数据。
- **改法**：
  1. 服务端：`audio_engine_core::audio_output::list_output_devices() -> Vec<(String, String, bool)>`（`audio_output.rs:160`，**已确认存在**，服务端 `routes.rs:1326` 已在使用 `AudioOutput`），包装为 `GET /api/v1/player/devices`；
  2. `httpClient.getOutputDevices` 映射该端点；
  3. 将 `OutputDeviceSelector.vue` 移入 `src/components/settings/custom/`，以 custom Item 接入 `player` 分类，`visible: () => isWebMode()`（与桌面端的设备下拉互斥显示）。
- **验收**：Web 模式设置 → 播放中可选 ALSA 硬件 PCM / Diretta 目标，切换后服务端实际输出变更。

### ENH-5 Web 更新检查入口语义化

- **改法**：`about` 分类的「检查更新」按钮 Web 下替换为「前往 Releases 页」（`window.open(RELEASES_URL)`），文案 i18n 增加 `settings.about.checkUpdateWeb`；`update` store 不参与。FIX-5 的 polyfill 事件模拟保留为兜底。

### ENH-6 背景图存储迁移

- **现状**：`theme.imageBackground.src` 以 dataURL 随 persist 写 localStorage（`theme.ts:151-164`），大图触发 5MB 配额异常。
- **改法**：背景图二进制存 IndexedDB（localforage `"theme"`），store 持久化只保留元信息（文件名/取色值）；加载时从 IndexedDB 读 blob 转 objectURL。桌面端同路径同样受益，非 Web 专属改动，单独提 PR。

---

## 5. 能力扩展路线（第二轮复核后重新定位）

> 首轮分析中的「不可用」判定混淆了「当前未实现」与「架构不可能」。第二轮逐项对照 `audio-engine-core` 与服务端能力复核后，约 15 项改判：多数只是「差一个服务端端点」，少数存在浏览器等价形态，真正架构性不可行的仅 5 类。本节为改判后的完整路线。

### 5.0 可实现性三档总表

**A 档 · 服务端可实现**（引擎能力现成或纯 HTTP，工程量以端点计）

| 功能 | 已核实的依据 | 定位 |
|---|---|---|
| FFT 频谱 | `fft_data()`（`player/mod.rs:622`）现成——**已落地**（`0e9d96d` WS 订阅转发） | SRV-1，✅ 完成 |
| EQ/变速/变调/淡入淡出/响度均衡 | Player `set_equalizer_enabled`(:661)、`set_equalizer_bands`(:675)、`set_speed`(:703)、`set_pitch`(:713)、`set_pitch_sync`(:722)、`set_fade_duration`(:586)、`set_normalization_enabled`(:644) 全部现成 | SRV-2，**P2** |
| 输出设备枚举/切换 | `list_output_devices()`（`audio_output.rs:160`），服务端已用 `AudioOutput`（`routes.rs:1326`） | ENH-4，P2 |
| Last.fm / ListenBrainz | 桌面实现为纯 HTTP（`lastfm/client.ts:8` 直连 `ws.audioscrobbler.com`）；ListenBrainz 开放 API | SRV-5，**P2** |
| 标签编辑 / 本地曲目删除 | 服务端文件操作（lofty 类 crate 写 ID3/Vorbis） | SRV-6，**P2** |
| 下载 | 服务端下载进曲库（对 headless 反而更有意义，丰富服务器曲库） | SRV-7，**P2** |
| 云盘上传 / 文件拖放导入 | 浏览器 `File` 读内容 → multipart POST 服务端 → 带 cookie 转发；「浏览器无文件路径通道」仅对桌面 API 形状成立，非根本限制 | SRV-8，**P2** |
| 播放统计聚合 | 播放记录已入库（`/api/v1/stats/record`），只差聚合查询 | ENH-1，P2 |
| 其余顺带项 | MCP（服务端本身即服务，可原生暴露）、检查更新、缓存管理、外置/本地歌词仓库、歌手头像预取 | 已在 ENH 区或随对应端点顺带落地 |

**B 档 · 浏览器等价形态**（非 OS 等价，用户价值接近）

| 功能 | 替代形态 | 定位 |
|---|---|---|
| 系统媒体控制 / 媒体键 | MediaSession API——前端当前**零使用**（全库 grep 无命中）；控制的是服务端播放，呈现的是用户本机 OS 媒体面板与硬件媒体键。比在无显示器服务端跑 MPRIS 更对症 | SRV-9，**P2**（纯前端，成本低） |
| 桌面歌词 | Document Picture-in-Picture（Chromium 116+，真置顶小窗歌词页） | SRV-10，P3 |
| orpheus:// 协议 | URL 参数深链（`?play=<id>` 等），Web 端协议唤起的合理替代 | SRV-10，P3 |
| UI 缩放 / 全屏 | CSS zoom / Fullscreen API | 不立项，浏览器原生能力已覆盖 |

**C 档 · 架构性不可行**（明确不做，避免反复再议）

| 功能 | 理由 |
|---|---|
| 任务栏歌词 | 服务端无显示器、无任务栏 |
| OS 级全局快捷键 | 浏览器安全模型不允许页面注册全局热键（媒体键除外，走 SRV-9） |
| 原生置顶桌面歌词窗口 | 服务端无桌面会话；PiP 是近似替代而非等价 |
| 托盘 / 窗口管理 | 同上 |
| 音源改写型插件 | 需要网络中间人位置，浏览器沙箱不可给；服务端嵌 JS 引擎（deno_core 等）架构可行，但工程量与插件安全面不成比例，明确不做（纯网络型插件未来可评估浏览器 Worker 方案） |

### 5.1 SRV-1 FFT 频谱推送（✅ 已落地，无需实施）

**已完成**：commit `0e9d96d`（2026-09-05）实现 WS 显式订阅转发——客户端 `ws.onopen` 发送 `{type:"subscribe", data:{fft:true}}`，服务端按连接计数维护 `fft_subscriber_count`（`state.rs:83-84`），`PlayerEvent::FftData` 经广播频道按订阅过滤转发（`state.rs:202-206`），计数归零自动关闭引擎 FFT 定时器避免无消费空转；`httpClient.handleWsMessage` 已有 `fftData` 分支（秒→毫秒对齐的 `seeked` 确认事件同 commit 落地）。

**残留收尾（并入批次 5 顺带验证）**：
- 验证 `events.ts` 的 `fftData → playback.setFftFrame` 喂入链路在 Web 端实际出波形；
- 如需「设置页频谱开关」，映射到订阅开关而非引擎开关即可。

### 5.2 SRV-2 DSP 端点（P2，自 P3 升级，依据已逐项确认）

- **服务端**新增（直接调用 Player 方法，行号见 5.0 表）：

  ```
  POST /api/v1/player/equalizer   { enabled, bands: number[], preamp }
  POST /api/v1/player/speed       { speed }
  POST /api/v1/player/pitch       { semitones, sync }
  POST /api/v1/player/fade        { ms }
  POST /api/v1/player/normalization { enabled }
  ```

- **前端**：`httpClient.ts:743-792` 的对应 stub 全部替换为真实调用（此前是「接受即成功」的空实现）；`initPlayer` 的启动同步调用（`core/player/index.ts:1082-1091`）无需改动，落地即生效。
- **联动**：UI-2 中 `player` 分类被隐藏的淡入淡出/响度均衡/EQ/变速设置项，随端点落地逐项恢复 `visible`；`status` 快照已含 `speed`（`state.rs:202`），前端插值时间源（`playback.setSpeed`）与引擎脱节的隐患同时消除。

### 5.3 SRV-5 Last.fm / ListenBrainz Scrobble（P2，新增）

- **服务端**：移植 `electron/main/services/lastfm/`（纯 HTTP client，`client.ts:73` 一个 `fetch` 封装即是核心）为 Rust 模块：auth session 换取 + scrobble + now playing；ListenBrainz 为 user token 直连开放 API，更简单。凭证入服务端加密 config（与 streaming.json 同路径策略）。
- **前端**：polyfill 的 `lastfm`/`listenbrainz` stub（`webPolyfill.ts:401-421`）替换为 HTTP 端点；`services` 分类对应设置项与收藏/播放联动（`useFavorite.ts:39` 的 love 丢失点）恢复。

### 5.4 SRV-6 标签编辑与本地曲目删除（P2，新增）

- **服务端**：`POST /api/v1/library/tags`（lofty 类 crate 写 ID3/FLAC/Vorbis 标签与内嵌封面）、`POST /api/v1/library/tracks/delete`（删除后触发增量重扫）；封面写入走上传临时文件。
- **前端**：polyfill `library.writeTags`（现为 safe proxy 默认失败）、`library.deleteTracks`（现为假成功 `webPolyfill.ts:285`）、`library.pickCoverImage` 映射端点；`TagEditorDialog` 与右键删除入口从 UI-3 隐藏清单中移除、恢复可用。

### 5.5 SRV-7 服务端下载进曲库（P2，新增）

- **设计**：服务端新增 `POST /api/v1/download/start { track, quality }`，复用现有音源解析（`/api/v1/proxy/apis/call` + 官方解析），按桌面端文件名模板落盘**到服务器曲库目录**并写标签（复用 SRV-6 能力），任务表入 SQLite，进度经 WS 新增 `download_progress` 事件（复用 `scan_progress` 同款通道模式，`routes.rs:2246-2254`）。
- **前端**：polyfill `download` stub 替换为端点映射；`DownloadList`/`useDownload` 恢复（`useDownload.ts:74-78` 的误导 toast 随真实返回值自然修正）；侧边栏 `/download` 入口改为跟随服务端能力（`download.enabled` 且 Web 模式）。
- **语义**：下载目标是服务器磁盘而非浏览器——headless 场景下「给曲库补货」比「存到访问设备」更符合定位；浏览器另存可用 Blob 方案作补充，不单列。

### 5.6 SRV-8 云盘上传与文件拖放导入（P2，新增）

- **云盘上传**：`POST /api/v1/cloud/upload`（multipart），服务端带网易 cookie 转发云盘上传接口；前端 `cloudUpload.ts` 的 `pickSongs` 改浏览器 file input、`uploadSong` 映射端点（现为 stub：入队即 error，`cloudUpload.ts:97-104`）。
- **拖放导入**：`useExternalFileHandler.ts` 改造——浏览器拖拽拿到 `File` 对象后 `POST /api/v1/library/import` 落盘到导入目录并触发扫描；替代桌面端 `getPathForFile`（绝对路径，Web 恒空）的语义。

### 5.7 SRV-9 MediaSession 媒体控制（P2，纯浏览器端）

- **改法**：在 `core/player` 事件层接入 `navigator.mediaSession`——`setActionHandler`（play/pause/previoustrack/nexttrack/seekto）转发到 `playerClient`，`setPositionState` 随 position 事件更新，metadata 随 track 更新。**仅 Web 模式启用**（桌面端媒体控制由主进程 media-ctrl 负责，避免双通道）。
- **价值**：用户本机 OS 媒体面板与硬件媒体键直接控制 headless 播放——这是 C 档「全局快捷键」在浏览器里唯一可达的子集，也是 headless 场景最自然的多端控制入口。

### 5.8 SRV-3 服务端识别（P3，维持可选）

- 链路已全部核实可行：浏览器 `getUserMedia` 采集就绪（`captureInRenderer`）；匹配本质是一次网易 HTTP 调用（`electron/main/services/recognition/match.ts:5` `callNetease`）；指纹计算（`fingerprint.ts`，wasm/worker）需移植 Rust 或浏览器端运行。落地后识曲入口从 UI-3 隐藏清单恢复。维持 P3 因涉及指纹移植选型。

### 5.9 SRV-4 流媒体凭证服务端存储（P3，维持可选，架构级）

- Web 端流媒体服务器配置改存服务端加密 config，消除 IndexedDB 明文密码（`web/storage.ts:17-25`）。`native/streaming-api` crate 可作服务端适配层基础；属「浏览器直连 → 服务端代理」架构迁移，收益是多端共享流媒体配置，需单独评审。

### 5.10 SRV-10 浏览器等价形态合集（P3）

- **PiP 歌词小窗**：Document Picture-in-Picture（Chromium 116+）打开独立歌词页，具备真置顶特性，作为桌面歌词的浏览器近似替代（Safari/Firefox 降级为普通弹窗）。
- **URL 深链**：`?play=<trackId>` / `?album=` / `?playlist=` 参数解析，复用 `orpheus.ts` 的载荷分发逻辑（其解析与执行本就是纯前端，仅触发源 `onProtocolUrl` 不可用），替代 orpheus:// 协议唤起。

---

## 6. 验证方案

1. **静态检查**：`pnpm typecheck`（web+node 双目标）、`pnpm lint`、Prettier 全量格式化。
2. **单元测试**：扩展 `src/services/client/client.spec.ts`——
   - FIX-3：hotkey `getAll` 默认返回 `defaultHotkeyConfig` 形状；`set` → `getAll` 读回一致；reset 单项/全部语义；
   - FIX-4：`getStatsSummary` 返回对象且字段全数值；
   - FIX-6：`cache.song.lookup` 返回 `null`；
   - FIX-5：`check()` 后 `onEvent` 监听器依次收到 `{type:"checking"}` 与 `{type:"notAvailable"}` 事件；
   - FIX-2：`recognition.isSupported()` 解析为 `false`。
3. **Web 手工回归**（`pnpm dev` + 浏览器访问 dev server，`/api`、`/ws` 代理到 14558，见 `electron.vite.config.ts:85-97`）：
   - 播放/暂停/切曲/进度/音量/歌词/歌单/音乐库扫描/流媒体/统计页逐项过一遍；
   - 按 UI-1/UI-2/UI-3 清单确认桌面控件全部不可见；
   - FIX-1：设置 → 关于不报错。
4. **桌面端回归**：`pnpm dev` 正常启动，重点抽查 hotkey 设置、更新检查、识曲、EQ 设置页——确认零行为变化（所有 polyfill/谓词改动在桌面端不生效）。
5. **打包链路**：`scripts/package-linux-headless.sh` 产物内 `web/` 为新 renderer，`systemctl` 部署后按 `docs/linux-headless-server/test-plan-and-report.md` 的验收路径复测。
6. **能力扩展项（§5，随各批次补充）**：
   - SRV-1（已落地，回归验证）：Web 端 WS 连接后频谱组件出现波形；断开后服务端 FFT 定时器随订阅计数归零自动关闭；
   - SRV-2：五个 DSP 端点 curl 往返 + Web 设置页改参后实际听感/切歌保持生效；
   - SRV-5：Last.fm 连接授权后播放一首曲目，last.fm 端出现 scrobble 记录；
   - SRV-6：Web 端改标签后 `ffprobe` 验证落盘、删除曲目后扫描数减少；
   - SRV-7：Web 端发起下载，服务器曲库目录出现带标签成品且扫描收录；
   - SRV-8：Web 端上传一首到云盘、拖放导入一首进曲库，两侧扫描均收录；
   - SRV-9：Web 播放时本机 OS 媒体面板显示曲目、媒体键可控制服务端播放；桌面端确认 MediaSession 未启用（无双通道）。

## 7. 实施顺序与工作量估算

| 批次 | 内容 | 预估 | 提交拆分建议 |
|---|---|---|---|
| 1 | FIX-1 ~ FIX-7 | 0.5 ~ 1 人日 | 每个 FIX 一个 commit（`fix: …`），全部只触 polyfill + 2 个调用点 |
| 2 | UI-0 ~ UI-3 | 1 ~ 2 人日 | `feat: 设置分类支持 visible`（schema+消费方）→ `feat: Web 模式裁剪桌面专属 UI`（各组件）分开提交 |
| 3 | ENH-1（方案 A 含 Rust）/ ENH-2（含 Rust）/ ENH-3 / ENH-5 | 2 ~ 3 人日 | 前端与 Rust 端点各自成 commit，Rust 侧遵循 `native/headless-server` 现有 handler 风格 |
| 4 | ENH-4 / ENH-6 | 1 ~ 1.5 人日 | ENH-4 含服务端 `GET /api/v1/player/devices`；ENH-6 独立 PR |
| 5 | SRV-2（DSP 端点 + 前端接线一体交付）；SRV-1 已由 `0e9d96d` 落地，仅回归验证 | 1 ~ 1.5 人日 | Rust 端点一个 commit，前端 `httpClient` 接线 + UI `visible` 恢复一个 commit |
| 6 | SRV-5 / SRV-6 / SRV-9 | 2 ~ 2.5 人日 | SRV-9 纯前端可先行；SRV-5/6 各含 Rust 模块单独成 commit |
| 7 | SRV-7 / SRV-8 | 3 ~ 4 人日 | 下载涉及任务表与 WS 通道，上传涉及 multipart 与导入目录约定，各自独立 PR |
| 8 | SRV-3 / SRV-4 / SRV-10 | 单独立项评估 | 指纹移植选型 / 流媒体架构迁移 / PiP 兼容矩阵，逐项评审后排期 |

> 与首轮版本的差异：原批次 5（SRV 全部 P3 单独立项）拆分如上——SRV-1/2 经能力核实后与常规增强同量级，SRV-9 为纯前端低成本项，均进入正式排期；C 档不可行项不再占用排期讨论。

---

## 8. 复核记录（2026-09-05）

对全文逐项对照源码二次核验，结论与修订如下。

### 修正的 4 处

| 位置 | 原方案问题 | 修订 |
|---|---|---|
| FIX-5 | **实质性错误**：把更新事件写成了 `onChecking/onUpdateAvailable/...` 分立监听器。实际协议是单一 `onEvent(cb)` 通道接收 `UpdateEvent` 联合类型（`electron/preload/index.ts:712-720`、`shared/types/update.ts:19-26`），原 stub 里的分立方法本身就是 store 从不订阅的死代码 | 已重写：单一 `onEvent` 通道 + `check` 时派发 `checking → notAvailable(manual)` 事件序列，`manual` 透传保证手动检查弹 toast |
| FIX-1 | 行号引用不完整：`versions` 除模板渲染外还是 `envItems` computed（:55-71）与「复制环境信息」（:73-76）的数据源 | 已补 `envItems` 条件展开代码与复制功能说明 |
| FIX-4 | 遗漏一个既有缺陷：polyfill 现有 `getPlayHistoryDaily`（:344-347）把原始播放记录直接返回给 `Stats.vue:28`，与 `DailyPlayStats[]`（`{day, playCount}`）形状不匹配，属「错数据」而非「空数据」 | 已纳入 FIX-4：P0 先规范化返回 `[]`，真实聚合归 ENH-1 |
| UI-1 | 组件路径写错且修法不优：`WindowControls.vue` 实际在 `src/components/layout/`（非 `ui/`），且组件根元素自身有 `v-if="isBorderless"`（:23） | 改为单点修复：根 `v-if` 追加 `&& !isWebMode()`，一处覆盖全部挂载点 |

### 复核通过的关键结论（抽样列举）

- 桌面端 `recognition:submitPcm` 返回 `{success:true}`（`electron/main/ipc/recognition.ts:28-30`），FIX-2 的兜底检查在桌面端不会误报。
- 桌面端 `cache.song.lookup` 返回 `Promise<string | null>`（`electron/preload/index.ts:528-529`），FIX-6 的 `async () => null` 语义精确对齐。
- `PlayStatsSummary` 共 8 个数值字段（`shared/types/stats.ts:28-45`），FIX-4 的全零对象与类型逐字段匹配，可通过 vue-tsc。
- `@shared/defaults/hotkeys` 导入路径有先例（`HotkeyConfig.vue:3` 导入同文件 `HOTKEY_ACTIONS`），FIX-3 的 import 可用。
- `manager.ts` 的 `recompile()` 只消费 `bindings[id].inApp`、不检查 `globalEnabled`，FIX-3「Web 下 inApp 快捷键可工作」的前提成立。
- `SettingsSearch.vue:29-33` 已有逐级 `visible` 过滤模式，UI-2 的 category 级扩展与其同构。
- UI 入口行号全部落实：右键菜单下载项谓词在 `useTrackMenu.ts:120`、误导 toast 在 `useDownload.ts:74-78`（stub 无 `reason` 必弹「已在队列」）、`LoginDialog.vue:49`、`cloudUpload.ts:139-142`（stub 无 `length` 静默不入队）、`TagEditorDialog` 的 `readTags`(:86)/`pickCoverImage`(:100)、`Toolbar.vue:119-128`、`NavSearch.vue:299,304`。
- ENH-1 依据坐实：`/api/v1/stats/summary` 返回 `get_library_stats`（`routes.rs:2220-2228`），与 `PlayStatsSummary` 是两回事；`DailyPlayStats`/`HourlyPlayStats` 类型已确认，可直接写入新端点契约。

### 第二轮复核（2026-09-05，能力可实现性）

用户质询「已判定 headless 无法使用的功能是否真的无法实现」，据此对全部「不可用/降级」项逐条对照引擎与服务端能力二次核验。**结论：首轮将约 15 项误归入「架构不可行」，实际多数为「当前未实现，可实现性良好」。**

**改判依据（关键证据）：**

- DSP 全家（EQ/变速/变调/淡入淡出/响度均衡）：`audio-engine-core` Player 方法逐一在场——`set_fade_duration`(:586)、`set_normalization_enabled`(:644)、`set_equalizer_enabled`(:661)、`set_equalizer_bands`(:675)、`set_speed`(:703)、`set_pitch`(:713)、`set_pitch_sync`(:722)；FFT `fft_data()`(:622)。headless-server 本就 wrap 该 Player（`routes.rs:1326`）→ SRV-1/2 自 P3 升级 P2。
- 设备枚举：`list_output_devices()`（`audio_output.rs:160`）现成 → ENH-4 的「前置确认」解除。
- Last.fm：桌面端实现即纯 HTTP client（`lastfm/client.ts:8,73`）→ 可整体移植服务端（SRV-5 新增 P2）。
- 云盘上传/拖放：首轮「浏览器无文件路径通道」的表述只对桌面 API 形状成立，`File` → multipart → 服务端转发无架构障碍（SRV-8 新增 P2）。
- 识别：采集（浏览器 `getUserMedia`）、匹配（`match.ts:5` 纯网易 HTTP）两段均无障碍，仅指纹移植待选型 → SRV-3 维持 P3 但依据坐实。
- 媒体控制：前端 MediaSession 全库零命中 → B 档替代形态成立（SRV-9 新增 P2）。

**维持不可行的（C 档，明确不做）：** 任务栏歌词、OS 级全局快捷键（媒体键除外）、原生置顶歌词窗口、托盘/窗口管理、音源改写型插件。

**文档层面联动：** §1 总表更新（SRV-1/2 升 P2，新增 SRV-5~10）；ENH-4 前置确认解除；§6 验证方案补能力项验收；§7 批次重排（原「SRV 单独立项」拆为批次 5~8）。

### 第三轮修订（2026-09-05，上游同步影响评估）

针对「修复是否影响 `git merge origin/dev` 同步上游」进行评估（当前状态：merge-base `ac0bcfa`，本地领先 61 commits、落后 0，历史上定期合并 origin/dev），两点结论落入正文：

1. **SRV-1 过时并标完成**：`0e9d96d`（会话期间并行落地）已实现 WS FFT 显式订阅转发 + `seeked` 确认事件 + `httpClient` 消费分支，§5.1 改为完成记录；首轮报告中「频谱不可用」的结论同步作废。
2. **UI-2 改为中心化隐藏清单**（§3）：原方案在 8 个 `settings/categories/*.ts`（上游活跃共享文件）逐项散布 `visible` 谓词，会产生几十个上游冲突点；改为 fork 独有的 `webHidden.ts` 清单 + 消费端 2~3 处单点过滤，上游分类文件零触碰。

**同步影响总体结论**：方案对上游共享文件的触碰均为单点受控改动（约 10 个文件、各 1~10 行），其余全部落在 fork 独有文件（`src/services/client/`、`native/headless-server/`、`src/web/`、`webHidden.ts`）；冲突风险低且可控。唯一超出本方案的既有成本：`native/audio-engine-core` 为 fork 自上游 `native/audio-engine` 拆分产物，上游重构 audio-engine 时需人工对齐——SRV-2 调用的 Player 方法若被上游改名，`headless-server` 编译期即暴露，属可感知风险。
