//! Linux Headless Server
//!
//! SPlayer 的纯 Rust HTTP/WebSocket 服务，提供播放控制、状态查询、扫描、元数据等 REST API
//! 以及实时状态推送的 WebSocket 连接。

#![allow(clippy::needless_collect)]

pub mod api;
pub mod config;
pub mod db;
pub mod error;
pub mod state;
