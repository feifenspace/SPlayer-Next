pub const TIDAL_API_BASE: &str = "https://api.tidal.com/v1";
pub const TIDAL_AUTH_BASE: &str = "https://auth.tidal.com/v1/oauth2";
pub const TIDAL_LOGIN_BASE: &str = "https://login.tidal.com";
pub const TIDAL_OPENAPI_BASE: &str = "https://openapi.tidal.com";

pub const TIDAL_DEFAULT_COUNTRY: &str = "US";
pub const TIDAL_SCOPE: &str = "r_usr+w_usr+w_sub";

/// 官方移动客户端 Client ID（公共客户端，无 secret）
/// 享受最高 Entitlement 权限：标准 LOSSLESS / Hi-Res 不被降级为 AAC。
pub const TIDAL_OFFICIAL_CLIENT_ID: &str = "YUJf8vfXOxVvzo2W";
pub const TIDAL_ANDROID_REDIRECT_URI: &str = "https://com.player.tidal/auth";

/// 备选 Client ID/Secret
pub const TIDAL_DEFAULT_CLIENT_ID: &str = "fX2JxdmntZWK0ixT";
pub const TIDAL_DEFAULT_CLIENT_SECRET: &str = "1Nn9AfDAjrrgJFJbKNWLeAyKHVGmINuXPPLHVXAvxAg=";

pub const TIDAL_USER_AGENT: &str = "TIDAL/6.2.0 (Android; 14; en-US)";
pub const TIDAL_V2_CONTENT_TYPE: &str = "application/vnd.tidal.v1+json";
