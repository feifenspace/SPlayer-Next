use thiserror::Error;

/// crate 统一错误类型。
#[derive(Debug, Error)]
pub enum QqkgError {
    /// 上游接口业务层失败（HTTP 层通畅但 code 非 200 等）。
    #[error("upstream api error: {0}")]
    Upstream(String),
    /// 上游响应结构不符合预期（字段缺失 / 类型错误）。
    #[error("bad upstream response: {0}")]
    BadResponse(String),
    /// 需要登录态但未找到已保存的凭据。
    #[error("missing credentials: {0}")]
    MissingCredentials(String),
    /// 参数不合法。
    #[error("invalid parameter: {0}")]
    InvalidParam(String),
    /// 序列化 / 反序列化失败。
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

