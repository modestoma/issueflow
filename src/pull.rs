use crate::{
    config::{Config, Platform},
    error::{Error, Result},
    service::Service,
    target::{Target, encode},
    transport::Transport,
};
use clap::ValueEnum;
use http::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePull {
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub head: String,
    pub base: String,
    #[serde(default)]
    pub draft: bool,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}
impl MergeMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}
pub fn target_from_url(config: &Config, input: &str) -> Result<Target> {
    let mut u = Url::parse(input).map_err(|_| Error::new("input", "Invalid PR URL"))?;
    let parts: Vec<_> = u.path().trim_matches('/').split('/').collect();
    if parts.len() != 4 || parts[2] != "pull" {
        return Err(Error::new("input", "Expected a GitHub pull request URL"));
    }
    let path = format!("/{}/{}/issues/{}", parts[0], parts[1], parts[3]);
    u.set_path(&path);
    let t = Target::from_url(config, u.as_str())?;
    github(&t)?;
    Ok(t)
}
fn github(t: &Target) -> Result<()> {
    if t.platform != Platform::Github {
        Err(Error::new(
            "input",
            "PR commands currently support GitHub only",
        ))
    } else {
        Ok(())
    }
}
fn branch(s: &str) -> Result<()> {
    if s.is_empty()
        || s == "@"
        || s.starts_with('-')
        || s.contains("..")
        || s.contains("@{")
        || s.chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c))
        || s.split('/')
            .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with('.') || p.ends_with(".lock"))
    {
        return Err(Error::new(
            "input",
            "Invalid branch name; use an explicit branch in the same repository",
        ));
    }
    Ok(())
}
fn partial(mut e: Error) -> Error {
    e.outcome_unknown = true;
    e.message = format!(
        "PR write may have succeeded; inspect remote state before retrying. {}",
        e.message
    );
    e
}
pub struct Pulls<'a> {
    pub transport: &'a dyn Transport,
    pub target: Target,
}
impl Pulls<'_> {
    fn collection(&self) -> Result<String> {
        github(&self.target)?;
        Ok(format!("repos/{}/pulls", self.target.repository))
    }
    fn endpoint(&self) -> Result<String> {
        Ok(format!(
            "{}/{}",
            self.collection()?,
            self.target
                .number
                .ok_or_else(|| Error::new("input", "PR URL required"))?
        ))
    }
    pub async fn show(&self) -> Result<Value> {
        let v = self
            .transport
            .request(Method::GET, &self.endpoint()?, None)
            .await?;
        if !v["number"].is_u64() || !v["head"]["sha"].is_string() || !v["base"]["ref"].is_string() {
            return Err(Error::new("response", "Incomplete PR response"));
        }
        Ok(v)
    }
    pub async fn list(&self, head: Option<&str>, base: Option<&str>) -> Result<Value> {
        if let Some(s) = head {
            branch(s)?;
        }
        if let Some(s) = base {
            branch(s)?;
        }
        let mut endpoint = format!(
            "{}?state=open&sort=created&direction=asc",
            self.collection()?
        );
        if let Some(h) = head {
            let owner = self.target.repository.split('/').next().unwrap();
            endpoint.push_str(&format!("&head={}", encode(&format!("{owner}:{h}"))));
        }
        if let Some(b) = base {
            endpoint.push_str(&format!("&base={}", encode(b)));
        }
        let service = Service {
            transport: self.transport,
            target: self.target.clone(),
        };
        Ok(json!(service.pages(&endpoint).await?))
    }
    pub async fn create(&self, input: CreatePull, issue_url: &str) -> Result<Value> {
        github(&self.target)?;
        branch(&input.head)?;
        branch(&input.base)?;
        if input.title.trim().is_empty() || input.head == input.base {
            return Err(Error::new(
                "input",
                "A title and distinct head/base branches are required",
            ));
        }
        Service {
            transport: self.transport,
            target: self.target.clone(),
        }
        .raw_issue()
        .await?;
        let matches = self.list(Some(&input.head), Some(&input.base)).await?;
        let matches: Vec<_> = matches
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| {
                v["head"]["ref"] == input.head
                    && v["base"]["ref"] == input.base
                    && v["head"]["repo"]["full_name"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&self.target.repository))
            })
            .collect();
        if matches.len() > 1 {
            return Err(Error::new(
                "conflict",
                "Multiple open PRs match these branches",
            ));
        }
        let marker = format!("Refs {issue_url}");
        if let Some(v) = matches.first() {
            if !v["body"]
                .as_str()
                .is_some_and(|b| b.lines().any(|l| l == marker))
            {
                return Err(Error::new(
                    "conflict",
                    "An open PR already uses these branches but does not reference this issue; inspect it before continuing",
                ));
            }
            return Ok(json!({"reused":true,"pull_request":v}));
        }
        let body = if input.body.lines().any(|l| l == marker) {
            input.body
        } else {
            format!("{}\n\n{marker}", input.body.trim_end())
        };
        let v=self.transport.request(Method::POST,&self.collection()?,Some(json!({"title":input.title,"body":body,"head":input.head,"base":input.base,"draft":input.draft}))).await?;
        let n = v["number"]
            .as_u64()
            .ok_or_else(|| partial(Error::new("response", "Created PR response has no number")))?;
        let mut t = self.target.clone();
        t.number = Some(n);
        let verified = Self {
            transport: self.transport,
            target: t,
        }
        .show()
        .await
        .map_err(partial)?;
        if verified["head"]["ref"] != input.head || verified["base"]["ref"] != input.base {
            return Err(partial(Error::new(
                "conflict",
                "PR branch readback differs",
            )));
        }
        Ok(json!({"reused":false,"pull_request":verified}))
    }
    pub async fn merge(&self, sha: &str, base: &str, method: MergeMethod) -> Result<Value> {
        branch(base)?;
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::new("input", "Expected a full 40-character head SHA"));
        }
        let v = self.show().await?;
        if v["state"] != "open" || v["draft"] != false || v["merged"] != false {
            return Err(Error::new(
                "conflict",
                "Only an open, non-draft, unmerged PR can be merged",
            ));
        }
        if v["head"]["sha"] != sha || v["base"]["ref"] != base {
            return Err(Error::new(
                "conflict",
                "PR head or target changed; review the latest PR before merging",
            ));
        }
        let result = self
            .transport
            .request(
                Method::PUT,
                &format!("{}/merge", self.endpoint()?),
                Some(json!({"sha":sha,"merge_method":method.as_str()})),
            )
            .await?;
        if result["merged"] != true {
            return Err(partial(Error::new(
                "conflict",
                "GitHub did not confirm the merge",
            )));
        }
        let verified = self.show().await.map_err(partial)?;
        if verified["merged"] != true || verified["state"] != "closed" {
            return Err(partial(Error::new(
                "response",
                "Merge could not be verified",
            )));
        }
        Ok(json!({"merged":true,"pull_request":verified}))
    }
}
