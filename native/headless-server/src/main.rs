//! 服务启动入口

use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tracing::info;

use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::state::AppState;

/// 服务默认监听地址 (默认绑定 0.0.0.0 允许局域网访问)
const DEFAULT_ADDR: &str = "0.0.0.0:14558";

/// 启动 HTTP 服务
pub async fn start_server(config: Config) -> Result<SocketAddr> {
    let addr = config
        .listen_addr
        .parse()
        .unwrap_or_else(|_| DEFAULT_ADDR.parse().expect("Invalid default address"));

    let state = AppState::new(&config)?;

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
                .unwrap_or("headless_server=info,axum=info"),
        )
        .init();

    let mut config = Config::load()?;
    let mut host = "0.0.0.0".to_string();
    let mut port = 14558u16;

    // 解析命令行参数
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--web-root" => {
                if i + 1 < args.len() {
                    config.web_root = Some(std::path::PathBuf::from(&args[i + 1]));
                    i += 1;
                }
            }
            "--listen" => {
                if i + 1 < args.len() {
                    config.listen_addr = args[i + 1].clone();
                    i += 1;
                }
            }
            "--host" => {
                if i + 1 < args.len() {
                    host = args[i + 1].clone();
                    i += 1;
                }
            }
            "--port" => {
                if i + 1 < args.len() {
                    if let Ok(p) = args[i + 1].parse() {
                        port = p;
                    }
                    i += 1;
                }
            }
            "--token" => {
                if i + 1 < args.len() {
                    config.api_token = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--data-dir" => {
                if i + 1 < args.len() {
                    let dir = std::path::PathBuf::from(&args[i + 1]);
                    config.database_path = Some(dir.join("library.db"));
                    config.cover_cache_dir = Some(dir.join("covers"));
                    i += 1;
                }
            }
            "--database-path" | "--db" => {
                if i + 1 < args.len() {
                    config.database_path = Some(std::path::PathBuf::from(&args[i + 1]));
                    i += 1;
                }
            }
            "--cover-dir" => {
                if i + 1 < args.len() {
                    config.cover_cache_dir = Some(std::path::PathBuf::from(&args[i + 1]));
                    i += 1;
                }
            }
            "--diretta-target" => {
                if i + 1 < args.len() {
                    config.diretta_target = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if config.listen_addr == "127.0.0.1:14558" || config.listen_addr.is_empty() {
        config.listen_addr = format!("{}:{}", host, port);
    }

    let _addr = start_server(config).await?;

    // 保持主线程存活
    tokio::signal::ctrl_c().await?;
    Ok(())
}
