//! Qobuz & TIDAL Hi-Res Streaming API client (pure Rust, zero Node.js runtime).

pub mod error;
pub mod qobuz;
pub mod tidal;

pub use error::StreamingError;
pub use qobuz::QobuzClient;
pub use tidal::TidalClient;
