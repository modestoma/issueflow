use std::{collections::HashMap, fmt, fs, path::Path};

use clap::{Args, ValueEnum};
use serde::Serialize;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Github,
    Gitlab,
}

#[derive(Args, Default)]
pub struct Overrides {
    #[arg(long, global = true)]
    pub platform: Option<Platform>,
    #[arg(long, global = true)]
    pub repository: Option<String>,
    #[arg(long, global = true)]
    pub github_api_url: Option<String>,
    #[arg(long, global = true)]
    pub gitlab_url: Option<String>,
    #[arg(long, global = true)]
    pub timeout_seconds: Option<u64>,
}

// Credentials are never serialized, printed, or accepted as command-line arguments.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Debug)]
pub struct Config {
    pub platform: Option<Platform>,
    pub repository: Option<String>,
    pub github_api_url: Url,
    pub gitlab_url: Option<Url>,
    pub github_token: Option<Secret>,
    pub gitlab_token: Option<Secret>,
    pub timeout_seconds: u64,
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Load one file, without searching parents or mutating the process environment.
/// dotenvy handles quoting, comments and variable expansion.
pub fn read_env_file(path: &Path, required: bool) -> Result<HashMap<String, String>, ConfigError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(HashMap::new());
        }
        Err(_) => return Err(ConfigError("无法读取指定的 .env 文件".into())),
    };
    let mut values = HashMap::new();
    for entry in dotenvy::from_read_iter(content.as_bytes()) {
        let (key, value) =
            entry.map_err(|_| ConfigError(".env 格式错误；未输出原始内容以保护凭据".into()))?;
        if key.starts_with("ISSUEFLOW_") && values.insert(key, value).is_some() {
            return Err(ConfigError(".env 包含重复的 ISSUEFLOW 配置项".into()));
        }
    }
    Ok(values)
}

impl Config {
    pub fn resolve(
        file: HashMap<String, String>,
        environment: HashMap<String, String>,
        flags: Overrides,
    ) -> Result<Self, ConfigError> {
        let mut merged = file;
        merged.extend(environment);
        let get = |key: &str| merged.get(key).cloned();
        let platform = match flags.platform {
            Some(platform) => Some(platform),
            None => match get("ISSUEFLOW_PLATFORM").as_deref() {
                None | Some("") => None,
                Some("github") => Some(Platform::Github),
                Some("gitlab") => Some(Platform::Gitlab),
                Some(_) => {
                    return Err(ConfigError(
                        "ISSUEFLOW_PLATFORM 必须为 github 或 gitlab".into(),
                    ));
                }
            },
        };
        let repository = flags
            .repository
            .or_else(|| get("ISSUEFLOW_REPOSITORY"))
            .filter(|s| !s.is_empty());
        if let Some(repo) = &repository {
            let segments: Vec<_> = repo.split('/').collect();
            if segments.len() < 2
                || segments.iter().any(|s| {
                    s.is_empty()
                        || *s == "."
                        || *s == ".."
                        || !s
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                })
                || (platform == Some(Platform::Github) && segments.len() != 2)
            {
                return Err(ConfigError(
                    "ISSUEFLOW_REPOSITORY 必须为有效的组/项目路径".into(),
                ));
            }
        }
        let timeout = match flags.timeout_seconds {
            Some(value) => value,
            None => get("ISSUEFLOW_TIMEOUT_SECONDS")
                .unwrap_or_else(|| "30".into())
                .parse::<u64>()
                .map_err(|_| ConfigError("ISSUEFLOW_TIMEOUT_SECONDS 必须为整数".into()))?,
        };
        if !(1..=300).contains(&timeout) {
            return Err(ConfigError("请求超时必须在 1 至 300 秒之间".into()));
        }
        let github_api_url = parse_url(
            &flags
                .github_api_url
                .or_else(|| get("ISSUEFLOW_GITHUB_API_URL"))
                .unwrap_or_else(|| "https://api.github.com".into()),
            "ISSUEFLOW_GITHUB_API_URL",
        )?;
        let gitlab_url = flags
            .gitlab_url
            .or_else(|| get("ISSUEFLOW_GITLAB_URL"))
            .filter(|s| !s.is_empty())
            .map(|value| parse_url(&value, "ISSUEFLOW_GITLAB_URL"))
            .transpose()?;
        let token = |key| get(key).filter(|s| !s.trim().is_empty()).map(Secret);
        Ok(Self {
            platform,
            repository,
            github_api_url,
            gitlab_url,
            github_token: token("ISSUEFLOW_GITHUB_TOKEN"),
            gitlab_token: token("ISSUEFLOW_GITLAB_TOKEN"),
            timeout_seconds: timeout,
        })
    }

    pub fn redacted(&self) -> serde_json::Value {
        serde_json::json!({
            "platform": self.platform,
            "repository": self.repository,
            "github_api_url": self.github_api_url.as_str(),
            "gitlab_url": self.gitlab_url.as_ref().map(Url::as_str),
            "github_token_configured": self.github_token.is_some(),
            "gitlab_token_configured": self.gitlab_token.is_some(),
            "timeout_seconds": self.timeout_seconds,
        })
    }
}

fn parse_url(value: &str, key: &str) -> Result<Url, ConfigError> {
    let error = || {
        ConfigError(format!(
            "{key} 必须为 HTTPS 地址，不能含凭据、查询参数或片段"
        ))
    };
    let mut url = Url::parse(value).map_err(|_| error())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(error());
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}
