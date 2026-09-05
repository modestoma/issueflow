use crate::{
    config::{Config, Overrides},
    error::{Error, Result},
    target::Target,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
fn config_for_url(value: &str) -> Result<Config> {
    let parsed = url::Url::parse(value)
        .map_err(|_| Error::new("input", "Invalid issue URL in branch contract"))?;
    let mut environment = HashMap::new();
    if value.contains("/-/issues/") || value.contains("/-/merge_requests/") {
        environment.insert(
            "ISSUEFLOW_GITLAB_URL".into(),
            format!(
                "{}://{}",
                parsed.scheme(),
                parsed
                    .host_str()
                    .ok_or_else(|| Error::new("input", "Invalid GitLab host in branch contract"))?
            ),
        );
    }
    Config::resolve(HashMap::new(), environment, Overrides::default()).map_err(Error::from)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchContract {
    pub schema_version: u32,
    pub issue_url: String,
    pub parent_issue_url: Option<String>,
    pub source_branch: String,
    pub branch: String,
    pub pr_target: String,
    pub pr_url: Option<String>,
}
impl BranchContract {
    fn basic(&self) -> Result<Target> {
        let invalid = || {
            Error::new(
                "input",
                "Invalid branch contract: use distinct development and target branches in one repository",
            )
        };
        if self.schema_version != 1
            || self.branch == self.pr_target
            || self.source_branch != self.pr_target
        {
            return Err(invalid());
        }
        for name in [&self.branch, &self.source_branch, &self.pr_target] {
            crate::pull::branch(name).map_err(|_| invalid())?;
        }
        let cfg = config_for_url(&self.issue_url)?;
        let issue = Target::from_url(&cfg, &self.issue_url)?;
        if let Some(url) = &self.pr_url {
            let pr = crate::pull::target_from_url(&cfg, url)?;
            if !pr.repository.eq_ignore_ascii_case(&issue.repository) {
                return Err(invalid());
            }
        }
        Ok(issue)
    }
    pub fn validate(&self, parent: Option<&BranchContract>) -> Result<Value> {
        let issue = self.basic()?;
        match (&self.parent_issue_url, parent) {
            (None, None) => {}
            (None, Some(_)) => {
                return Err(Error::new(
                    "input",
                    "Root contract cannot receive a parent contract",
                ));
            }
            (Some(_), None) => {
                return Err(Error::new(
                    "input",
                    "Child contract requires --parent-file for target validation",
                ));
            }
            (Some(url), Some(parent)) => {
                let p = parent.basic()?;
                let cfg = config_for_url(url)?;
                let expected = Target::from_url(&cfg, url)?;
                if expected != p
                    || issue == p
                    || !issue.repository.eq_ignore_ascii_case(&p.repository)
                    || self.pr_target != parent.branch
                {
                    return Err(Error::new(
                        "conflict",
                        "Child must reference its parent and target the parent's integration branch in the same repository",
                    ));
                }
            }
        }
        Ok(
            json!({"valid":true,"issue_url":self.issue_url,"branch":self.branch,"pr_target":self.pr_target,"relationship":if parent.is_some(){"child"}else{"root"},"remote_verified":false,"note":"Local contract validation only. Inspect actual PR head/base and delivery before merging; this does not prove remote branches exist or validate the entire ancestor graph."}),
        )
    }
}
