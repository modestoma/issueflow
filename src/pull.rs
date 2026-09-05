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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePull {
    pub title: Option<String>,
    pub body: Option<String>,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum PullState {
    Open,
    Closed,
    All,
    Merged,
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
pub(crate) fn branch(s: &str) -> Result<()> {
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
        self.list_state(head, base, PullState::Open).await
    }
    pub async fn list_state(
        &self,
        head: Option<&str>,
        base: Option<&str>,
        state: PullState,
    ) -> Result<Value> {
        if let Some(s) = head {
            branch(s)?;
        }
        if let Some(s) = base {
            branch(s)?;
        }
        let mut endpoint = format!(
            "{}?state={}&sort=created&direction=asc",
            self.collection()?,
            match state {
                PullState::Open => "open",
                PullState::Closed | PullState::Merged => "closed",
                PullState::All => "all",
            }
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
        let mut items = service.pages(&endpoint).await?;
        if matches!(state, PullState::Merged) {
            items.retain(|p| p["merged_at"].is_string());
        }
        Ok(json!(items))
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
    fn check_edit_head(v: &Value, sha: &str) -> Result<()> {
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::new("input", "Expected a full head SHA"));
        }
        if v["state"] != "open" || v["merged"] != false || v["head"]["sha"] != sha {
            return Err(Error::new(
                "conflict",
                "PR closed or head changed; inspect before updating",
            ));
        }
        Ok(())
    }
    pub async fn update(&self, input: UpdatePull, sha: &str) -> Result<Value> {
        if input.title.is_none() && input.body.is_none() {
            return Err(Error::new("input", "No PR update fields"));
        }
        if input.title.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Err(Error::new("input", "PR title cannot be empty"));
        }
        let current = self.show().await?;
        Self::check_edit_head(&current, sha)?;
        let mut payload = json!({});
        if let Some(title) = input.title {
            payload["title"] = json!(title);
        }
        if let Some(mut body) = input.body {
            for line in current["body"]
                .as_str()
                .unwrap_or("")
                .lines()
                .filter(|l| l.starts_with("Refs https://"))
            {
                if !body.lines().any(|l| l == line) {
                    body.push_str(&format!("\n\n{line}"));
                }
            }
            payload["body"] = json!(body);
        }
        self.transport
            .request(Method::PATCH, &self.endpoint()?, Some(payload.clone()))
            .await?;
        let verified = self.show().await.map_err(partial)?;
        for (key, value) in payload.as_object().unwrap() {
            if verified[key] != *value {
                return Err(partial(Error::new(
                    "conflict",
                    "PR metadata readback differs",
                )));
            }
        }
        if verified["head"]["sha"] != sha {
            return Err(partial(Error::new(
                "conflict",
                "PR head changed during update",
            )));
        }
        Ok(verified)
    }
    pub async fn ready(&self, sha: &str) -> Result<Value> {
        let current = self.show().await?;
        Self::check_edit_head(&current, sha)?;
        if current["draft"] == false {
            return Ok(json!({"changed":false,"pull_request":current}));
        }
        if current["draft"] != true {
            return Err(Error::new("response", "Missing PR draft state"));
        }
        let id = current["node_id"]
            .as_str()
            .ok_or_else(|| Error::new("response", "Missing PR node ID"))?;
        let v=self.transport.request(Method::POST,"graphql",Some(json!({"query":"mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){pullRequest{id}}}","variables":{"id":id}}))).await?;
        if v.get("errors")
            .is_some_and(|e| !e.as_array().is_some_and(|a| a.is_empty()))
            || !v["data"]["markPullRequestReadyForReview"]["pullRequest"]["id"].is_string()
        {
            return Err(partial(Error::new(
                "graphql",
                "Unable to confirm ready-for-review mutation",
            )));
        }
        let verified = self.show().await.map_err(partial)?;
        if verified["draft"] != false || verified["head"]["sha"] != sha {
            return Err(partial(Error::new(
                "conflict",
                "PR readiness readback differs",
            )));
        }
        Ok(json!({"changed":true,"pull_request":verified}))
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
