use serde::Serialize;
use std::fmt;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub outcome_unknown: bool,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            status: None,
            outcome_unknown: false,
        }
    }
    pub fn http(status: u16, write: bool) -> Self {
        let code = match status {
            401 => "authentication",
            403 => "permission",
            404 => "not_found",
            409 | 412 => "conflict",
            429 => "rate_limited",
            300..=399 => "redirect",
            _ => "api",
        };
        Self {
            code,
            message: format!("API 返回 HTTP {status}；未自动重试，请核对权限、目标与服务状态"),
            status: Some(status),
            outcome_unknown: write && status >= 500,
        }
    }
    pub fn network(write: bool) -> Self {
        Self {
            outcome_unknown: write,
            ..Self::new(
                "network",
                "请求失败或超时；写入结果可能未知，请先查询远端，勿盲目重发",
            )
        }
    }
    pub fn exit_code(&self) -> u8 {
        match self.code {
            "input" | "configuration" => 2,
            "authentication" | "permission" => 3,
            "not_found" => 4,
            "conflict" => 5,
            _ => 1,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<crate::config::ConfigError> for Error {
    fn from(value: crate::config::ConfigError) -> Self {
        Self::new("configuration", value.to_string())
    }
}
pub type Result<T> = std::result::Result<T, Error>;
