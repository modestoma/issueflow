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
    let path = if let Some(base) = &config.gitlab_url
        && u.origin() == base.origin()
        && let Some(path) = u.path().strip_prefix(base.path())
        && let Some((repo, number)) = path.trim_end_matches('/').rsplit_once("/-/merge_requests/")
    {
        format!("{}{repo}/-/issues/{number}", base.path())
    } else {
        let parts: Vec<_> = u.path().trim_matches('/').split('/').collect();
        if parts.len() != 4 || parts[2] != "pull" {
            return Err(Error::new(
                "input",
                "Expected a configured GitHub PR or GitLab MR URL",
            ));
        }
        format!("/{}/{}/issues/{}", parts[0], parts[1], parts[3])
    };
    u.set_path(&path);
    Target::from_url(config, u.as_str())
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
        "PR/MR write may have succeeded; inspect remote state before retrying. {}",
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
        Ok(match self.target.platform {
            Platform::Github => format!("repos/{}/pulls", self.target.repository),
            Platform::Gitlab => format!(
                "projects/{}/merge_requests",
                encode(&self.target.repository)
            ),
        })
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
        let complete = match self.target.platform {
            Platform::Github => {
                v["number"].is_u64() && v["head"]["sha"].is_string() && v["base"]["ref"].is_string()
            }
            Platform::Gitlab => {
                v["iid"].is_u64()
                    && v["sha"].is_string()
                    && v["target_branch"].is_string()
                    && v["source_branch"].is_string()
            }
        };
        if !complete {
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
            "{}?state={}{}",
            self.collection()?,
            match (self.target.platform, state) {
                (Platform::Github, PullState::Open) => "open",
                (Platform::Github, PullState::Closed | PullState::Merged) => "closed",
                (Platform::Github, PullState::All) => "all",
                (Platform::Gitlab, PullState::Open) => "opened",
                (Platform::Gitlab, PullState::Closed) => "closed",
                (Platform::Gitlab, PullState::Merged) => "merged",
                (Platform::Gitlab, PullState::All) => "all",
            },
            if self.target.platform == Platform::Github {
                "&sort=created&direction=asc"
            } else {
                "&order_by=created_at&sort=asc"
            }
        );
        if let Some(h) = head {
            match self.target.platform {
                Platform::Github => {
                    let owner = self.target.repository.split('/').next().unwrap();
                    endpoint.push_str(&format!("&head={}", encode(&format!("{owner}:{h}"))));
                }
                Platform::Gitlab => endpoint.push_str(&format!("&source_branch={}", encode(h))),
            }
        }
        if let Some(b) = base {
            endpoint.push_str(&format!(
                "&{}={}",
                if self.target.platform == Platform::Github {
                    "base"
                } else {
                    "target_branch"
                },
                encode(b)
            ));
        }
        let service = Service {
            transport: self.transport,
            target: self.target.clone(),
        };
        let mut items = service.pages(&endpoint).await?;
        if self.target.platform == Platform::Github && matches!(state, PullState::Merged) {
            items.retain(|p| p["merged_at"].is_string());
        }
        Ok(json!(items))
    }
    pub async fn create(&self, input: CreatePull, issue_url: &str) -> Result<Value> {
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
            .filter(|v| match self.target.platform {
                Platform::Github => {
                    v["head"]["ref"] == input.head
                        && v["base"]["ref"] == input.base
                        && v["head"]["repo"]["full_name"]
                            .as_str()
                            .is_some_and(|s| s.eq_ignore_ascii_case(&self.target.repository))
                }
                Platform::Gitlab => {
                    v["source_branch"] == input.head
                        && v["target_branch"] == input.base
                        && v["source_project_id"] == v["target_project_id"]
                }
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
            let body_key = if self.target.platform == Platform::Github {
                "body"
            } else {
                "description"
            };
            if !v[body_key]
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
        let payload = match self.target.platform {
            Platform::Github => {
                json!({"title":input.title,"body":body,"head":input.head,"base":input.base,"draft":input.draft})
            }
            Platform::Gitlab => {
                json!({"title":if input.draft { format!("Draft: {}", input.title) } else { input.title },"description":body,"source_branch":input.head,"target_branch":input.base})
            }
        };
        let v = self
            .transport
            .request(Method::POST, &self.collection()?, Some(payload))
            .await?;
        let n = v[if self.target.platform == Platform::Github {
            "number"
        } else {
            "iid"
        }]
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
        if self.head_branch(&verified) != Some(input.head.as_str())
            || self.base_branch(&verified) != Some(input.base.as_str())
        {
            return Err(partial(Error::new(
                "conflict",
                "PR branch readback differs",
            )));
        }
        Ok(json!({"reused":false,"pull_request":verified}))
    }
    fn head_sha<'a>(&self, v: &'a Value) -> Option<&'a str> {
        v[if self.target.platform == Platform::Github {
            "head"
        } else {
            "diff_refs"
        }][if self.target.platform == Platform::Github {
            "sha"
        } else {
            "head_sha"
        }]
        .as_str()
        .or_else(|| v["sha"].as_str())
    }
    fn head_branch<'a>(&self, v: &'a Value) -> Option<&'a str> {
        v[if self.target.platform == Platform::Github {
            "head"
        } else {
            "source_branch"
        }][if self.target.platform == Platform::Github {
            "ref"
        } else {
            ""
        }]
        .as_str()
        .or_else(|| v["source_branch"].as_str())
    }
    fn base_branch<'a>(&self, v: &'a Value) -> Option<&'a str> {
        v[if self.target.platform == Platform::Github {
            "base"
        } else {
            "target_branch"
        }][if self.target.platform == Platform::Github {
            "ref"
        } else {
            ""
        }]
        .as_str()
        .or_else(|| v["target_branch"].as_str())
    }
    fn draft(&self, v: &Value) -> Option<bool> {
        if self.target.platform == Platform::Github {
            v["draft"].as_bool()
        } else {
            v["draft"]
                .as_bool()
                .or_else(|| v["work_in_progress"].as_bool())
        }
    }
    fn merged(&self, v: &Value) -> bool {
        if self.target.platform == Platform::Github {
            v["merged"] == true
        } else {
            v["state"] == "merged" || v["merged_at"].is_string()
        }
    }
    fn open(&self, v: &Value) -> bool {
        v["state"]
            == if self.target.platform == Platform::Github {
                "open"
            } else {
                "opened"
            }
    }
    fn check_edit_head(&self, v: &Value, sha: &str) -> Result<()> {
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::new("input", "Expected a full head SHA"));
        }
        if !self.open(v) || self.merged(v) || self.head_sha(v) != Some(sha) {
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
        self.check_edit_head(&current, sha)?;
        let mut payload = json!({});
        if let Some(title) = input.title {
            payload["title"] = json!(title);
        }
        if let Some(mut body) = input.body {
            let body_key = if self.target.platform == Platform::Github {
                "body"
            } else {
                "description"
            };
            for line in current[body_key]
                .as_str()
                .unwrap_or("")
                .lines()
                .filter(|l| l.starts_with("Refs https://"))
            {
                if !body.lines().any(|l| l == line) {
                    body.push_str(&format!("\n\n{line}"));
                }
            }
            payload[body_key] = json!(body);
        }
        self.transport
            .request(
                if self.target.platform == Platform::Github {
                    Method::PATCH
                } else {
                    Method::PUT
                },
                &self.endpoint()?,
                Some(payload.clone()),
            )
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
        if self.head_sha(&verified) != Some(sha) {
            return Err(partial(Error::new(
                "conflict",
                "PR head changed during update",
            )));
        }
        Ok(verified)
    }
    pub async fn ready(&self, sha: &str) -> Result<Value> {
        let current = self.show().await?;
        self.check_edit_head(&current, sha)?;
        if self.draft(&current) == Some(false) {
            return Ok(json!({"changed":false,"pull_request":current}));
        }
        if self.draft(&current) != Some(true) {
            return Err(Error::new("response", "Missing PR draft state"));
        }
        if self.target.platform == Platform::Gitlab {
            let title = current["title"]
                .as_str()
                .ok_or_else(|| Error::new("response", "Missing MR title"))?;
            let title = title
                .strip_prefix("Draft:")
                .or_else(|| title.strip_prefix("WIP:"))
                .map(str::trim)
                .ok_or_else(|| Error::new("response", "Unable to remove GitLab draft prefix"))?;
            self.transport
                .request(Method::PUT, &self.endpoint()?, Some(json!({"title":title})))
                .await?;
            let verified = self.show().await.map_err(partial)?;
            if self.draft(&verified) != Some(false) || self.head_sha(&verified) != Some(sha) {
                return Err(partial(Error::new(
                    "conflict",
                    "MR readiness readback differs",
                )));
            }
            return Ok(json!({"changed":true,"pull_request":verified}));
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
        if self.draft(&verified) != Some(false) || self.head_sha(&verified) != Some(sha) {
            return Err(partial(Error::new(
                "conflict",
                "PR readiness readback differs",
            )));
        }
        Ok(json!({"changed":true,"pull_request":verified}))
    }
    pub async fn merge(&self, sha: &str, base: &str, method: MergeMethod) -> Result<Value> {
        branch(base)?;
        if self.target.platform == Platform::Gitlab && matches!(method, MergeMethod::Rebase) {
            return Err(Error::new(
                "input",
                "GitLab MR merge does not support the rebase method",
            ));
        }
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::new("input", "Expected a full 40-character head SHA"));
        }
        let v = self.show().await?;
        if !self.open(&v) || self.draft(&v) != Some(false) || self.merged(&v) {
            return Err(Error::new(
                "conflict",
                "Only an open, non-draft, unmerged PR can be merged",
            ));
        }
        if self.head_sha(&v) != Some(sha) || self.base_branch(&v) != Some(base) {
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
                Some(if self.target.platform == Platform::Github {
                    json!({"sha":sha,"merge_method":method.as_str()})
                } else {
                    json!({"sha":sha,"squash":matches!(method, MergeMethod::Squash)})
                }),
            )
            .await?;
        if self.target.platform == Platform::Github && result["merged"] != true
            || self.target.platform == Platform::Gitlab && result["state"] != "merged"
        {
            return Err(partial(Error::new(
                "conflict",
                "The platform did not confirm the PR/MR merge",
            )));
        }
        let verified = self.show().await.map_err(partial)?;
        if !self.merged(&verified) {
            return Err(partial(Error::new(
                "response",
                "Merge could not be verified",
            )));
        }
        Ok(json!({"merged":true,"pull_request":verified}))
    }
}
