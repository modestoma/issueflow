use crate::{
    config::{Config, Overrides, Platform},
    error::{Error, Result},
    project::ProjectTarget,
    target::valid_repository,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    pub schema_version: u32,
    pub platform: String,
    pub host: String,
    pub repository: String,
    pub remote: String,
    pub base_branch: String,
    pub timezone: String,
    pub proposer: Option<String>,
    #[serde(default)]
    pub verification_commands: Vec<String>,
    #[serde(default)]
    pub manual_acceptance: Vec<String>,
    pub delivery_condition: Option<String>,
    #[serde(default)]
    pub delivery_policy: DeliveryPolicy,
    pub permissions: Permissions,
    pub github_project_url: Option<String>,
    #[serde(default)]
    pub branch_prefixes: HashMap<String, String>,
}
#[derive(Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPolicy {
    Merged,
    #[default]
    AcceptanceRequired,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub local_commit: bool,
    pub push: bool,
    pub pull_request: Option<bool>,
    pub draft_pr_mr: Option<bool>,
}
impl WorkflowConfig {
    pub fn validate(&self) -> Result<Value> {
        let invalid = || {
            Error::new(
                "configuration",
                "Invalid GitHub workflow configuration; check schema, repository, branch and Project fields",
            )
        };
        if self.schema_version != 1
            || self.platform != "github"
            || self.host != "github.com"
            || self.timezone != "Asia/Shanghai"
            || !valid_repository(&self.repository, Platform::Github)
        {
            return Err(invalid());
        }
        if self.remote.is_empty()
            || self.remote.starts_with('-')
            || !self
                .remote
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return Err(invalid());
        }
        crate::pull::branch(&self.base_branch).map_err(|_| invalid())?;
        for prefix in self.branch_prefixes.values() {
            if prefix.is_empty()
                || !prefix
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(invalid());
            }
        }
        if let Some(url) = &self.github_project_url {
            let cfg = Config::resolve(HashMap::new(), HashMap::new(), Overrides::default())
                .map_err(Error::from)?;
            ProjectTarget::parse(&cfg, url).map_err(|_| invalid())?;
        }
        Ok(
            json!({"valid":true,"schema_version":1,"repository":self.repository,"project":self.github_project_url,"delivery_policy":self.delivery_policy,"permissions":self.permissions,"note":"Validation does not execute commands, grant authorization, contact GitHub or load credentials."}),
        )
    }
}
