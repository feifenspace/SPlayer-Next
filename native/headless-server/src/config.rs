//! 配置模块

use serde::Deserialize;
use std::path::PathBuf;

/// 服务配置
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// 监听地址（如 127.0.0.1:14558）
    pub listen_addr: String,
    /// CORS 白名单（逗号分隔）
    pub cors_origins: Option<String>,
    /// API Token（可选，为空则不校验）
    pub api_token: Option<String>,
    /// 封面缓存目录
    pub cover_cache_dir: Option<PathBuf>,
    /// 数据库路径
    pub database_path: Option<PathBuf>,
    /// 静态 Web UI 托管根目录（如 ./out/renderer）
    pub web_root: Option<PathBuf>,
    /// 默认连接的 Diretta Target 目标地址（如 fe80::5241:b9ff:fe70:f9d2%2）
    pub diretta_target: Option<String>,
    /// 流媒体出网代理（如 http://127.0.0.1:7890 或 http://192.168.31.46:7890）
    pub proxy: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:14558".to_string(),
            cors_origins: None,
            api_token: None,
            cover_cache_dir: None,
            database_path: None,
            web_root: None,
            diretta_target: None,
            proxy: None,
        }
    }
}

impl Config {
    /// 从 YAML 文件加载配置并合并环境变量
    pub fn load() -> anyhow::Result<Self> {
        let mut paths = vec![
            "config/config.yaml",
            "config/config.yml",
            "/opt/splayer-headless/config/config.yaml",
            "/opt/splayer-headless/config/config.yml",
            "config.yaml",
            "config.yml",
            "/etc/splayer-headless/config.yaml",
            "/etc/splayer/config.yaml",
        ];

        let custom_path = std::env::var("SPLAYER_CONFIG_PATH").ok();
        if let Some(ref p) = custom_path {
            paths.insert(0, p.as_str());
        }

        let mut config = Self::default();

        for path in paths {
            if std::fs::metadata(path).is_ok() {
                let content = std::fs::read_to_string(path)?;
                config = serde_yaml::from_str(&content)?;
                break;
            }
        }

        // 环境变量覆盖
        if let Ok(addr) = std::env::var("SPLAYER_LISTEN_ADDR") {
            config.listen_addr = addr;
        }
        if let Ok(token) = std::env::var("SPLAYER_API_TOKEN") {
            config.api_token = Some(token);
        }
        if let Ok(data_dir) = std::env::var("SPLAYER_DATA_DIR") {
            let dir = PathBuf::from(data_dir);
            config.database_path = Some(dir.join("library.db"));
            config.cover_cache_dir = Some(dir.join("covers"));
        }
        if let Ok(db_path) = std::env::var("SPLAYER_DB_PATH") {
            config.database_path = Some(PathBuf::from(db_path));
        }
        if let Ok(cover_dir) = std::env::var("SPLAYER_COVER_DIR") {
            config.cover_cache_dir = Some(PathBuf::from(cover_dir));
        }
        if let Ok(diretta) = std::env::var("SPLAYER_DIRETTA_TARGET") {
            config.diretta_target = Some(diretta);
        }
        if let Ok(proxy) = std::env::var("SPLAYER_PROXY")
            .or_else(|_| std::env::var("QOBUZ_PROXY"))
            .or_else(|_| std::env::var("HTTPS_PROXY"))
            .or_else(|_| std::env::var("https_proxy"))
            .or_else(|_| std::env::var("ALL_PROXY"))
            .or_else(|_| std::env::var("all_proxy"))
        {
            config.proxy = Some(proxy);
        }

        if let Some(ref p) = config.proxy {
            if !p.is_empty() {
                std::env::set_var("QOBUZ_PROXY", p);
            }
        }

        Ok(config)
    }

    /// 解析最终使用的数据库文件路径（默认：<可执行文件目录>/data/library.db）
    pub fn resolved_database_path(&self) -> PathBuf {
        self.database_path
            .clone()
            .unwrap_or_else(|| default_data_dir().join("library.db"))
    }

    /// 解析最终使用的封面缓存目录（默认：<可执行文件目录>/data/covers）
    pub fn resolved_cover_cache_dir(&self) -> PathBuf {
        self.cover_cache_dir
            .clone()
            .unwrap_or_else(|| default_data_dir().join("covers"))
    }

    /// 获取 CORS 白名单列表
    pub fn cors_origins(&self) -> Vec<String> {
        self.cors_origins
            .as_deref()
            .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_else(|| vec!["http://localhost:5173".to_string(), "*".to_string()])
    }
}

/// 判断是否为 cargo 构建产物目录（target/{debug,release}[/deps]）
/// 开发运行不应把数据写入 target/（会被 cargo clean 清除）
fn is_cargo_target_dir(dir: &std::path::Path) -> bool {
    let comps: Vec<_> = dir.components().map(|c| c.as_os_str()).collect();
    comps
        .windows(2)
        .any(|w| w[0] == std::ffi::OsStr::new("target") && (w[1] == "debug" || w[1] == "release"))
}

/// 默认数据目录：锚定可执行文件所在目录（<exe_dir>/data），
/// 保证手动启动与 systemd 服务无论 CWD 如何都落在同一份数据上，
/// 更新软件时只要替换二进制与 web 即可，登录态与设置不丢失。
/// 开发运行（cargo 构建产物目录）回退 CWD 相对路径。
fn default_data_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if !is_cargo_target_dir(dir) {
                return dir.join("data");
            }
        }
    }
    // 开发回退：CWD 相对路径（保持历史行为）
    if std::path::Path::new("splayer-headless").is_file() {
        PathBuf::from("data")
    } else {
        PathBuf::from("splayer-headless/data")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_database_path_overrides_default() {
        let config = Config {
            database_path: Some(PathBuf::from("/custom/library.db")),
            ..Config::default()
        };
        assert_eq!(config.resolved_database_path(), PathBuf::from("/custom/library.db"));
    }

    #[test]
    fn explicit_cover_dir_overrides_default() {
        let config = Config {
            cover_cache_dir: Some(PathBuf::from("/custom/covers")),
            ..Config::default()
        };
        assert_eq!(config.resolved_cover_cache_dir(), PathBuf::from("/custom/covers"));
    }

    #[test]
    fn cargo_target_dirs_are_detected() {
        assert!(is_cargo_target_dir(std::path::Path::new(
            "/proj/native/headless-server/target/release"
        )));
        assert!(is_cargo_target_dir(std::path::Path::new(
            "/proj/native/headless-server/target/debug/deps"
        )));
        assert!(!is_cargo_target_dir(std::path::Path::new("/opt/splayer-headless")));
        assert!(!is_cargo_target_dir(std::path::Path::new(
            "/opt/splayer-headless/data"
        )));
    }

    #[test]
    fn default_data_dir_skips_cargo_target() {
        // 测试二进制位于 target/debug/deps，应回退 CWD 相对路径而非写入 target/
        let dir = default_data_dir();
        assert!(!is_cargo_target_dir(&dir), "数据目录不应落在 cargo target 内: {dir:?}");
    }
}
