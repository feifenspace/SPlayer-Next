//! 服务启动入口

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;

use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::state::AppState;

/// SPlayer-Next Headless Hi-Fi 音频服务
#[derive(Debug, Parser)]
#[command(name = "splayer-headless", version, about)]
struct Cli {
    /// 静态 Web UI 根目录（缺省自动探测）
    #[arg(long, value_name = "DIR")]
    web_root: Option<PathBuf>,
    /// 监听地址（如 192.168.1.10:14558，设置后忽略 --host/--port）
    #[arg(long, value_name = "ADDR")]
    listen: Option<String>,
    /// 监听主机（仅在 listen_addr 为默认值时生效）
    #[arg(long, value_name = "HOST", default_value = "0.0.0.0")]
    host: String,
    /// 监听端口（仅在 listen_addr 为默认值时生效）
    #[arg(long, value_name = "PORT", default_value_t = 14558)]
    port: u16,
    /// API Token（可选，为空则不校验）
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,
    /// 数据目录（library.db 与 covers 的父目录）
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// 数据库文件路径
    #[arg(long, value_name = "PATH", alias = "db")]
    database_path: Option<PathBuf>,
    /// 封面缓存目录
    #[arg(long, value_name = "DIR")]
    cover_dir: Option<PathBuf>,
    /// 默认连接的 Diretta Target 地址
    #[arg(long, value_name = "TARGET")]
    diretta_target: Option<String>,
}

impl Cli {
    /// 参数覆盖配置文件（参数名与历史版本完全兼容）
    fn apply_to(self, config: &mut Config) {
        if let Some(web_root) = self.web_root {
            config.web_root = Some(web_root);
        }
        if let Some(listen) = self.listen {
            config.listen_addr = listen;
        }
        if let Some(token) = self.token {
            config.api_token = Some(token);
        }
        if let Some(dir) = self.data_dir {
            config.database_path = Some(dir.join("library.db"));
            config.cover_cache_dir = Some(dir.join("covers"));
        }
        if let Some(db) = self.database_path {
            config.database_path = Some(db);
        }
        if let Some(cover) = self.cover_dir {
            config.cover_cache_dir = Some(cover);
        }
        if let Some(target) = self.diretta_target {
            config.diretta_target = Some(target);
        }
        // 历史 quirk 保持：--host/--port 仅在 listen_addr 为默认值/空时生效
        if config.listen_addr == "127.0.0.1:14558" || config.listen_addr.is_empty() {
            config.listen_addr = format!("{}:{}", self.host, self.port);
        }
    }
}

/// 启动 HTTP 服务
pub async fn start_server(config: Config) -> Result<SocketAddr> {
    // 解析失败直接报错退出：静默回退默认地址会让写错的配置静默绑定全网卡
    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("listen_addr 配置无效 '{}': {e}", config.listen_addr))?;

    let state = AppState::new(&config)?;

    headless_server::api::routes::spawn_output_recovery_watchdog(state.clone());

    let app = build_router(state);

    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {}", addr))?;

    info!("Headless server listening on {}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("Server failed");
    });

    Ok(addr)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .as_deref()
                .unwrap_or("headless_server=info,audio_engine_core=info,axum=warn"),
        )
        .init();

    let args = Cli::parse();
    let mut config = Config::load()?;
    args.apply_to(&mut config);
    audio_engine_core::priority::configure_rt_priority(i32::from(config.audio.rt_priority));

    let _addr = start_server(config).await?;

    // 保持主线程存活
    tokio::signal::ctrl_c().await?;
    Ok(())
}
