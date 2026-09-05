use crate::{
    branch_contract::BranchContract,
    config::{Config, Platform},
    error::{Error, Result},
    project::{ProjectTarget, Projects},
    pull::{PullState, Pulls, target_from_url},
    service::{CloseReason, Service},
    target::{Target, encode},
    transport::Transport,
    workflow_config::{DeliveryPolicy, WorkflowConfig},
};
use http::Method;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize, Default, Clone)]
pub struct Snapshot {
    pub issue_state: String,
    pub issue_reason: Option<String>,
    pub has_resolution: bool,
    pub resolution: Option<String>,
    pub project_blocked: bool,
    pub metadata_ready: bool,
    pub pr_state: Option<String>,
    pub pr_merged: bool,
    pub pr_draft: bool,
    pub pr_url: Option<String>,
    pub head_sha: Option<String>,
    pub delivered: bool,
    pub project_configured: bool,
    pub in_project: bool,
    pub project_status: Option<String>,
    pub done_option_available: bool,
}
#[derive(Serialize, Debug)]
pub struct Plan {
    pub phase: &'static str,
    pub actions: Vec<&'static str>,
    pub blockers: Vec<&'static str>,
}
pub fn plan(s: &Snapshot, policy: DeliveryPolicy, accepted: bool) -> Plan {
    let stop = |phase, reason| Plan {
        phase,
        actions: vec![],
        blockers: vec![reason],
    };
    if !s.project_configured {
        return stop("setup_required", "github_project_required");
    }
    if !s.metadata_ready {
        return stop("setup_required", "project_workflow_fields_required");
    }
    if s.project_blocked {
        return stop("delivery_pending", "project_blocked");
    }
    if !matches!(s.issue_state.as_str(), "open" | "closed")
        || s.pr_state
            .as_deref()
            .is_some_and(|v| !matches!(v, "open" | "closed"))
        || (s.pr_merged && s.pr_state.as_deref() != Some("closed"))
    {
        return stop("manual_review", "invalid_native_state_evidence");
    }
    if s.has_resolution
        || (s.issue_state == "closed" && s.issue_reason.as_deref() != Some("completed"))
    {
        return stop(
            "manual_review",
            "issue_terminated_or_closure_reason_unknown",
        );
    }
    if s.pr_state.is_none() {
        return stop("no_pr", "no_matching_pull_request");
    }
    if !s.pr_merged {
        if s.pr_state.as_deref() == Some("closed") {
            return stop("manual_review", "pull_request_closed_without_merge");
        }
        return stop(
            if s.pr_draft || s.project_status.as_deref() == Some("In progress") {
                "in_progress"
            } else {
                "in_review"
            },
            "pull_request_not_merged",
        );
    }
    if !s.delivered {
        return stop("delivery_pending", "merge_not_reachable_from_target");
    }
    if matches!(policy, DeliveryPolicy::AcceptanceRequired) && !accepted {
        return stop("acceptance_pending", "human_acceptance_required");
    }
    if s.project_configured && !s.done_option_available {
        return stop("manual_review", "project_done_option_missing_or_ambiguous");
    }
    let mut actions = Vec::new();
    if s.project_configured {
        if !s.in_project {
            actions.push("add_to_project");
        }
        if s.project_status.as_deref() != Some("Done") {
            actions.push("set_project_done");
        }
    }
    if s.resolution.as_deref() != Some("Completed") {
        actions.push("set_resolution_completed");
    }
    if s.issue_state != "closed" {
        actions.push("close_issue");
    }
    Plan {
        phase: if actions.is_empty() {
            "complete"
        } else {
            "reconciliation_needed"
        },
        actions,
        blockers: vec![],
    }
}
fn bad() -> Error {
    Error::new("response", "Incomplete workflow recovery evidence")
}
fn sha(v: &Value) -> Result<&str> {
    v.as_str()
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(bad)
}
pub struct Recovery<'a> {
    pub config: &'a Config,
    pub transport: &'a dyn Transport,
    pub contract: &'a BranchContract,
    pub parent: Option<&'a BranchContract>,
    pub workflow: &'a WorkflowConfig,
}
impl Recovery<'_> {
    fn target(&self) -> Result<Target> {
        self.workflow.validate()?;
        self.contract.validate(self.parent)?;
        let t = Target::from_url(self.config, &self.contract.issue_url)?;
        if t.platform != Platform::Github
            || !t.repository.eq_ignore_ascii_case(&self.workflow.repository)
        {
            return Err(Error::new(
                "configuration",
                "Contract repository does not match workflow configuration",
            ));
        }
        Ok(t)
    }
    fn project(&self) -> Result<Option<Projects<'_>>> {
        self.workflow
            .github_project_url
            .as_ref()
            .map(|url| {
                Ok(Projects {
                    transport: self.transport,
                    target: ProjectTarget::parse(self.config, url)?,
                })
            })
            .transpose()
    }
    pub async fn inspect(&self, accepted: bool) -> Result<(Snapshot, Plan)> {
        let t = self.target()?;
        let issue = Service {
            transport: self.transport,
            target: t.clone(),
        }
        .raw_issue()
        .await?;
        let mut s = Snapshot {
            issue_state: issue["state"].as_str().ok_or_else(bad)?.into(),
            issue_reason: issue["state_reason"].as_str().map(String::from),
            ..Default::default()
        };
        let pr_target = if let Some(url) = &self.contract.pr_url {
            Some(target_from_url(self.config, url)?)
        } else {
            let list = Pulls {
                transport: self.transport,
                target: t.clone(),
            }
            .list_state(
                Some(&self.contract.branch),
                Some(&self.contract.pr_target),
                PullState::All,
            )
            .await?;
            let candidates: Vec<_> = list
                .as_array()
                .ok_or_else(bad)?
                .iter()
                .filter(|p| {
                    p["head"]["ref"] == self.contract.branch
                        && p["base"]["ref"] == self.contract.pr_target
                        && p["head"]["repo"]["full_name"]
                            .as_str()
                            .is_some_and(|r| r.eq_ignore_ascii_case(&t.repository))
                })
                .collect();
            if candidates.len() > 1 {
                return Err(Error::new(
                    "conflict",
                    "Multiple historical PRs match; record an explicit pr_url in the contract",
                ));
            }
            if let Some(p) = candidates.first() {
                let mut target = t.clone();
                target.number = Some(p["number"].as_u64().ok_or_else(bad)?);
                Some(target)
            } else {
                None
            }
        };
        if let Some(pr_target) = pr_target {
            if !pr_target.repository.eq_ignore_ascii_case(&t.repository) {
                return Err(Error::new(
                    "input",
                    "PR repository differs from issue repository",
                ));
            }
            let pr = Pulls {
                transport: self.transport,
                target: pr_target,
            }
            .show()
            .await?;
            if pr["head"]["ref"] != self.contract.branch
                || pr["base"]["ref"] != self.contract.pr_target
                || !pr["head"]["repo"]["full_name"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&t.repository))
            {
                return Err(Error::new(
                    "conflict",
                    "Actual PR branches differ from the issue contract",
                ));
            }
            let reference = format!("Refs {}", self.contract.issue_url);
            if !pr["body"]
                .as_str()
                .unwrap_or("")
                .lines()
                .any(|l| l == reference)
            {
                return Err(Error::new(
                    "conflict",
                    "PR does not contain the contract's issue reference",
                ));
            }
            s.pr_state = Some(pr["state"].as_str().ok_or_else(bad)?.into());
            s.pr_merged = pr["merged"].as_bool().ok_or_else(bad)?;
            s.pr_draft = pr["draft"].as_bool().ok_or_else(bad)?;
            s.head_sha = Some(sha(&pr["head"]["sha"])?.into());
            s.pr_url = Some(pr["html_url"].as_str().ok_or_else(bad)?.into());
            if s.pr_merged {
                if s.pr_state.as_deref() != Some("closed") {
                    return Err(bad());
                }
                let merge = sha(&pr["merge_commit_sha"])?;
                let compare = self
                    .transport
                    .request(
                        Method::GET,
                        &format!(
                            "repos/{}/compare/{merge}...{}",
                            t.repository,
                            encode(&self.contract.pr_target)
                        ),
                        None,
                    )
                    .await?;
                s.delivered = matches!(compare["status"].as_str(), Some("ahead" | "identical"))
                    && compare["merge_base_commit"]["sha"].as_str() == Some(merge);
            }
        }
        if let Some(project) = self.project()? {
            s.project_configured = true;
            let value = project.items().await?;
            let fields = value["project"]["fields"].as_array().ok_or_else(bad)?;
            let statuses: Vec<_> = fields
                .iter()
                .filter(|f| f["name"] == "Status" && f["options"].is_array())
                .collect();
            s.done_option_available = statuses.len() == 1
                && statuses[0]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|o| o["name"] == "Done")
                    .count()
                    == 1;
            s.metadata_ready = ["Work type", "Priority", "Blocked", "Resolution"]
                .iter()
                .all(|name| {
                    fields
                        .iter()
                        .filter(|f| f["name"] == *name && f["options"].is_array())
                        .count()
                        == 1
                });
            let node = issue["node_id"].as_str().ok_or_else(bad)?;
            let matches: Vec<_> = value["items"]
                .as_array()
                .ok_or_else(bad)?
                .iter()
                .filter(|i| i["content"]["id"].as_str() == Some(node))
                .collect();
            if matches.len() > 1 {
                return Err(Error::new(
                    "conflict",
                    "Multiple Project memberships match the issue",
                ));
            }
            if let Some(item) = matches.first() {
                if item["isArchived"] == true {
                    return Err(Error::new(
                        "conflict",
                        "Project item is archived; inspect before recovery",
                    ));
                }
                s.resolution = item["resolution"]["name"].as_str().map(String::from);
                s.has_resolution = s.resolution.as_deref().is_some_and(|r| r != "Completed");
                s.project_blocked = item["blocked"]["name"].as_str().is_some_and(|b| b != "No");
                s.in_project = true;
                s.project_status = item["fieldValueByName"]["name"].as_str().map(String::from);
            }
        }
        let p = plan(&s, self.workflow.delivery_policy, accepted);
        Ok((s, p))
    }
    pub async fn reconcile(
        &self,
        accepted: bool,
        apply: bool,
        expected_sha: Option<&str>,
    ) -> Result<Value> {
        let (before, proposal) = self.inspect(accepted).await?;
        if !apply {
            return Ok(json!({"applied":false,"snapshot":before,"plan":proposal}));
        }
        let expected = expected_sha
            .ok_or_else(|| Error::new("input", "--apply requires --expected-head-sha"))?;
        if before.head_sha.as_deref() != Some(expected) {
            return Err(Error::new(
                "conflict",
                "PR head differs from approved recovery expectation",
            ));
        }
        if !proposal.blockers.is_empty() {
            return Ok(json!({"applied":false,"snapshot":before,"plan":proposal}));
        }
        let mut completed = Vec::new();
        for action in &proposal.actions {
            // Refresh evidence before every mutation, never replay a stale multi-step plan.
            let (latest, p) = self
                .inspect(accepted)
                .await
                .map_err(|e| partial(e, &completed))?;
            if latest.head_sha.as_deref() != Some(expected) || !p.blockers.is_empty() {
                return Err(partial(
                    Error::new(
                        "conflict",
                        "Recovery evidence changed; inspect before continuing",
                    ),
                    &completed,
                ));
            }
            if !p.actions.contains(action) {
                continue;
            }
            let result = match *action {
                "add_to_project" => self.project()?.ok_or_else(bad)?.add(&self.target()?).await,
                "set_project_done" => {
                    self.project()?
                        .ok_or_else(bad)?
                        .status(&self.target()?, Some("Done"))
                        .await
                }
                "set_resolution_completed" => {
                    self.project()?
                        .ok_or_else(bad)?
                        .field(&self.target()?, "Resolution", Some("Completed"), false)
                        .await
                }
                "close_issue" => {
                    Service {
                        transport: self.transport,
                        target: self.target()?,
                    }
                    .native_state(Some(CloseReason::Completed))
                    .await
                }
                _ => return Err(bad()),
            };
            result.map_err(|e| partial(e, &completed))?;
            completed.push(*action);
        }
        let (after, remaining) = self
            .inspect(accepted)
            .await
            .map_err(|e| partial(e, &completed))?;
        if !remaining.actions.is_empty() || !remaining.blockers.is_empty() {
            return Err(partial(
                Error::new(
                    "conflict",
                    "Recovery readback is not complete; inspect again",
                ),
                &completed,
            ));
        }
        Ok(
            json!({"applied":!completed.is_empty(),"completed_actions":completed,"snapshot":after,"plan":remaining}),
        )
    }
}
fn partial(mut e: Error, completed: &[&str]) -> Error {
    if !completed.is_empty() {
        e.outcome_unknown = true;
        e.message = format!("Completed steps: {}. {}", completed.join(", "), e.message);
    }
    e
}
