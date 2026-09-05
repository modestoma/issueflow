use crate::{
    config::{Config, Platform},
    error::{Error, Result},
};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub platform: Platform,
    pub repository: String,
    pub number: Option<u64>,
}

pub fn valid_repository(repo: &str, platform: Platform) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() >= 2
        && (platform != Platform::Github || parts.len() == 2)
        && parts.iter().all(|s| {
            !s.is_empty()
                && *s != "."
                && *s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        })
}

impl Target {
    pub fn defaults(config: &Config) -> Result<Self> {
        let platform = config.platform.ok_or_else(|| {
            Error::new(
                "configuration",
                "创建和列表操作需要 --platform 或 ISSUEFLOW_PLATFORM",
            )
        })?;
        let repo = config.repository.clone().ok_or_else(|| {
            Error::new("configuration", "需要 --repository 或 ISSUEFLOW_REPOSITORY")
        })?;
        if !valid_repository(&repo, platform) {
            return Err(Error::new("input", "仓库路径无效"));
        }
        Ok(Self {
            platform,
            repository: repo,
            number: None,
        })
    }
    pub fn from_url(config: &Config, input: &str) -> Result<Self> {
        let url = Url::parse(input).map_err(|_| Error::new("input", "issue URL 无效"))?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
        {
            return Err(Error::new(
                "input",
                "需要不含凭据或查询参数的 HTTPS issue URL",
            ));
        }
        let mut candidates = Vec::new();
        let gh_host = if config.github_api_url.host_str() == Some("api.github.com") {
            "github.com"
        } else {
            config.github_api_url.host_str().unwrap_or("")
        };
        if url.host_str() == Some(gh_host)
            && url.port_or_known_default() == config.github_api_url.port_or_known_default()
        {
            let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
            if parts.len() == 4 && parts[2] == "issues" {
                candidates.push((
                    Platform::Github,
                    format!("{}/{}", parts[0], parts[1]),
                    parts[3].to_string(),
                ));
            }
        }
        if let Some(base) = &config.gitlab_url
            && url.origin() == base.origin()
            && let Some(path) = url.path().strip_prefix(base.path())
            && let Some((repo, number)) = path.trim_end_matches('/').rsplit_once("/-/issues/")
        {
            candidates.push((Platform::Gitlab, repo.to_string(), number.to_string()));
        }
        if candidates.len() != 1 {
            return Err(Error::new(
                "input",
                "链接不属于已配置的平台主机，或不是唯一可识别的 issue 链接",
            ));
        }
        let (platform, repository, number) = candidates.remove(0);
        let number = number
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::new("input", "issue 原生编号无效"))?;
        if !valid_repository(&repository, platform) {
            return Err(Error::new("input", "issue 仓库路径无效"));
        }
        Ok(Self {
            platform,
            repository,
            number: Some(number),
        })
    }
    pub fn collection(&self) -> String {
        match self.platform {
            Platform::Github => format!("repos/{}/issues", self.repository),
            Platform::Gitlab => format!("projects/{}/issues", encode(&self.repository)),
        }
    }
    pub fn endpoint(&self) -> Result<String> {
        Ok(format!(
            "{}/{}",
            self.collection(),
            self.number
                .ok_or_else(|| Error::new("input", "此操作需要 issue URL"))?
        ))
    }
}
pub fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
