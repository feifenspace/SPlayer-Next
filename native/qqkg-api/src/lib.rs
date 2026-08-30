//! QQ 音乐 / 酷狗音乐在线 API 客户端（headless 独立 crate）。
//!
//! 纯 Rust 实现，零 Node 运行时依赖；桌面端 Electron/TS 实现仅作移植参考，
//! 加密层输出与桌面端逐字节一致（由对照单测向量锁定）。

pub mod crypto;
pub mod error;
pub mod kugou;
pub mod normalize;
pub mod qqmusic;
pub mod types;

pub use error::QqkgError;
pub use kugou::KugouClient;
pub use qqmusic::QqmusicClient;
pub use types::{
    KugouQrCheckResponse, KugouQrKeyResponse, PlatformProfile, SearchParams, SearchType,
    UserDetailResponse, KG_APPID, KG_CLIENTVER,
};

