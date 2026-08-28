//! MPEG-4 DST (Direct Stream Transfer) 纯 Rust 无损解码模块

pub mod ac;
pub mod decoder;

pub use ac::ArithmeticDecoder;
pub use decoder::{DstDecoder, BYTES_PER_FRAME_CHANNEL, SAMPLES_PER_FRAME_CHANNEL};
