/** 是否为开发环境 */
export const isDev = import.meta.env.MODE === "development" || import.meta.env.DEV;

/** 操作系统平台
 * 注意：window.api 由 Electron preload 注入；纯 Web 部署下由 webPolyfill 在 main chunk 中延迟注入。
 * 本模块会被拆分为独立 chunk，加载顺序无法保证早于 polyfill，因此必须可选链回退，否则模块顶层直接抛 TypeError 导致白屏卡 logo。
 * 回退值与 webPolyfill 的 detectPlatform/installType 默认值保持一致（Headless 部署即 Linux）。
 */
const platform = window.api?.system?.platform ?? "linux";
/** 是否为 Windows 系统 */
export const isWin = platform === "win32";
/** 是否为 macOS 系统 */
export const isMac = platform === "darwin";
/** 是否为 Linux 系统 */
export const isLinux = platform === "linux";

/** 应用版本号 */
export const APP_VERSION = __APP_VERSION__;

/** 安装类型（回退 portable 与 webPolyfill 一致） */
export const INSTALL_TYPE = window.api?.system?.installType ?? "portable";
/** 是否为 AppX 安装 */
export const IS_APPX = INSTALL_TYPE === "appx";
/** 仓库地址 */
export const REPO_URL = __APP_REPO_URL__;
/** 项目名称 */
export const REPO_NAME = __APP_REPO_NAME__;
/** 版权署名 */
export const COPYRIGHT_HOLDER = __APP_AUTHOR__;
/** 官网地址 */
export const HOMEPAGE_URL = __APP_HOMEPAGE__;
/** 作者主页 */
export const AUTHOR_URL = __APP_AUTHOR_URL__;
/** Git 提交哈希 */
export const COMMIT_HASH = __COMMIT_HASH__;
/** Git 提交日期 */
export const COMMIT_DATE = __COMMIT_DATE__;
