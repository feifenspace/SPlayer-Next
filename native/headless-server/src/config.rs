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
        }
    }
}

impl Config {
    /// 从 YAML 文件加载配置并合并环境变量
    pub fn load() -> anyhow::Result<Self> {
        let paths = ["config.yaml", "config.yml", "/etc/splayer/config.yaml"];

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

        Ok(config)
    }

    /// 解析最终使用的数据库文件路径（默认：data/library.db）
    pub fn resolved_database_path(&self) -> PathBuf {
        self.database_path.clone().unwrap_or_else(|| {
            if std::path::Path::new("splayer-headless").is_file() {
                PathBuf::from("data/library.db")
            } else {
                PathBuf::from("splayer-headless/data/library.db")
            }
        })
    }

    /// 解析最终使用的封面缓存目录（默认：data/covers）
    pub fn resolved_cover_cache_dir(&self) -> PathBuf {
        self.cover_cache_dir.clone().unwrap_or_else(|| {
            if std::path::Path::new("splayer-headless").is_file() {
                PathBuf::from("data/covers")
            } else {
                PathBuf::from("splayer-headless/data/covers")
            }
        })
    }

    /// 获取 CORS 白名单列表
    pub fn cors_origins(&self) -> Vec<String> {
        self.cors_origins
            .as_deref()
            .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_else(|| vec!["http://localhost:5173".to_string(), "*".to_string()])
    }
}
