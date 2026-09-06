//! 兼容导入路径：实现已拆分至 `api` 各子模块，此文件仅保留既有外部引用路径。
pub use super::{build_router, spawn_isolated_blocking, spawn_output_recovery_watchdog};
