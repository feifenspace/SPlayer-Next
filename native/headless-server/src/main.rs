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
    /// 监听主机（显式传参时无条件覆盖配置文件 listen_addr 的主机部分）
    #[arg(long, value_name = "HOST")]
    host: Option<String>,
    /// 监听端口（显式传参时无条件覆盖配置文件 listen_addr 的端口部分）
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,
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
        let listen_given = self.listen.is_some();
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
        // 显式 CLI 传参无条件覆盖配置文件。历史实现里配置文件的 listen_addr
        // 会静默吞掉 --host/--port（实测传 --host 127.0.0.1 --port 14799 仍绑
        // 配置的 0.0.0.0:14558），导致运维显式收窄监听面时实际不生效。
        // 无任何传参时保持配置文件值（默认 127.0.0.1:14558，仅回环）。
        // --listen 已给出时独占生效，忽略 --host/--port
        if !listen_given && (self.host.is_some() || self.port.is_some()) {
            let (host, port) = config
                .listen_addr
                .rsplit_once(':')
                .map(|(h, p)| (h.to_string(), p.to_string()))
                .unwrap_or_else(|| ("127.0.0.1".to_string(), "14558".to_string()));
            config.listen_addr = format!(
                "{}:{}",
                self.host.unwrap_or(host),
                self.port
                    .map(|p| p.to_string())
                    .unwrap_or(port)
            );
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
                // 自定义 target（diretta_handoff/diretta_dsd）不匹配按模块路径
                // 的指令，会被 EnvFilter 静默丢弃——排查 handoff/DSD 问题时
                // 无需再手动 RUST_LOG 重启，默认即可见
                .unwrap_or(
                    "headless_server=info,audio_engine_core=info,diretta_handoff=debug,diretta_dsd=info,axum=warn",
                ),
        )
        .init();

    let args = Cli::parse();
    let mut config = Config::load()?;
    args.apply_to(&mut config);
    audio_engine_core::priority::configure_rt_priority(i32::from(config.audio.rt_priority));

    #[cfg(target_os = "linux")]
    warn_if_diretta_lacks_rt(&config);

    let _addr = start_server(config).await?;

    // 保持主线程存活
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// Diretta 选中时校验实时调度能力：THRED_MODE(5) 的 SDK 工作线程依赖
/// SCHED_FIFO，无权限时 SDK 日志表现为 "Worker Thread Priority set Error →
/// connectWait 0"——跨格式切歌的全量重连必然失败（同格式 handoff 不受影响，
/// 症状呈"偶发切歌失败"）。systemd unit 已配置 LimitRTPRIO/AmbientCapabilities；
/// 手动运行需先执行 scripts/rt-tuning.sh
#[cfg(target_os = "linux")]
fn warn_if_diretta_lacks_rt(config: &Config) {
    if config.diretta_target.is_none() {
        return;
    }
    let policy = unsafe { libc::sched_getscheduler(0) };
    if policy != libc::SCHED_FIFO {
        tracing::warn!(
            policy,
            "当前进程未运行在 SCHED_FIFO 实时调度下：Diretta Target 可能拒绝时钟锁定（connectWait 失败）。请通过 systemd 服务运行（unit 已含 LimitRTPRIO/AmbientCapabilities）或先执行 scripts/rt-tuning.sh"
        );
    }
}
