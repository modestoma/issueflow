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
    pub label_workflow: bool,
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
impl Plan {
    fn allows(&self, action: &str) -> bool {
        self.actions.contains(&action)
            && (self.blockers.is_empty()
                || (self
                    .blockers
                    .iter()
                    .all(|reason| *reason == "human_acceptance_required")
                    && matches!(action, "add_to_project" | "set_project_done")))
    }
}
pub fn plan(s: &Snapshot, policy: DeliveryPolicy, accepted: bool) -> Plan {
    let stop = |phase, reason| Plan {
        phase,
        actions: vec![],
        blockers: vec![reason],
    };
    if !s.label_workflow && !s.project_configured {
        return stop("setup_required", "github_project_required");
    }
    if !s.label_workflow && !s.metadata_ready {
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
    if !s.label_workflow && s.project_configured && !s.done_option_available {
        return stop("manual_review", "project_done_option_missing_or_ambiguous");
    }
    let mut actions = Vec::new();
    if !s.label_workflow && s.project_configured {
        if !s.in_project {
            actions.push("add_to_project");
        }
        if s.project_status.as_deref() != Some("Done") {
            actions.push("set_project_done");
        }
    }
    if matches!(policy, DeliveryPolicy::AcceptanceRequired) && !accepted {
        return Plan {
            phase: "acceptance_pending",
            actions,
            blockers: vec!["human_acceptance_required"],
        };
    }
    if !s.label_workflow && s.resolution.as_deref() != Some("Completed") {
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
        if t.platform != self.workflow.platform()?
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
            label_workflow: t.platform == Platform::Gitlab,
            issue_state: match issue["state"].as_str().ok_or_else(bad)? {
                "opened" => "open".into(),
                value => value.into(),
            },
            issue_reason: issue["state_reason"].as_str().map(String::from),
            ..Default::default()
        };
        if t.platform == Platform::Gitlab {
            let labels = issue["labels"].as_array().ok_or_else(bad)?;
            let names: Vec<_> = labels.iter().filter_map(Value::as_str).collect();
            let stages: Vec<_> = names
                .iter()
                .filter(|name| name.starts_with("workflow::"))
                .collect();
            if stages.len() != 1 {
                return Err(Error::new(
                    "conflict",
                    "GitLab issue must have exactly one workflow stage label",
                ));
            }
            s.project_blocked = names.contains(&"blocked");
            s.has_resolution = names.iter().any(|name| name.starts_with("resolution::"));
            s.metadata_ready = true;
            s.done_option_available = true;
            s.project_configured = true;
            s.in_project = true;
            s.project_status = Some((*stages[0]).to_string());
            if names.contains(&"workflow::已完成") {
                s.resolution = Some("Completed".into());
                s.issue_reason = Some("completed".into());
            }
        }
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
                .filter(|p| match t.platform {
                    Platform::Github => {
                        p["head"]["ref"] == self.contract.branch
                            && p["base"]["ref"] == self.contract.pr_target
                            && p["head"]["repo"]["full_name"]
                                .as_str()
                                .is_some_and(|r| r.eq_ignore_ascii_case(&t.repository))
                    }
                    Platform::Gitlab => {
                        p["source_branch"] == self.contract.branch
                            && p["target_branch"] == self.contract.pr_target
                            && p["source_project_id"] == p["target_project_id"]
                    }
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
                target.number = Some(
                    p[if t.platform == Platform::Github {
                        "number"
                    } else {
                        "iid"
                    }]
                    .as_u64()
                    .ok_or_else(bad)?,
                );
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
            let branches_match = match t.platform {
                Platform::Github => {
                    pr["head"]["ref"] == self.contract.branch
                        && pr["base"]["ref"] == self.contract.pr_target
                        && pr["head"]["repo"]["full_name"]
                            .as_str()
                            .is_some_and(|r| r.eq_ignore_ascii_case(&t.repository))
                }
                Platform::Gitlab => {
                    pr["source_branch"] == self.contract.branch
                        && pr["target_branch"] == self.contract.pr_target
                        && pr["source_project_id"] == pr["target_project_id"]
                }
            };
            if !branches_match {
                return Err(Error::new(
                    "conflict",
                    "Actual PR branches differ from the issue contract",
                ));
            }
            let reference = format!("Refs {}", self.contract.issue_url);
            if !pr[if t.platform == Platform::Github {
                "body"
            } else {
                "description"
            }]
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
            s.pr_state = Some(
                match pr["state"].as_str().ok_or_else(bad)? {
                    "opened" => "open",
                    "merged" => "closed",
                    value => value,
                }
                .into(),
            );
            s.pr_merged = if t.platform == Platform::Github {
                pr["merged"].as_bool().ok_or_else(bad)?
            } else {
                pr["state"] == "merged" && pr["merged_at"].is_string()
            };
            s.pr_draft = pr["draft"]
                .as_bool()
                .or_else(|| pr["work_in_progress"].as_bool())
                .ok_or_else(bad)?;
            s.head_sha = Some(
                sha(if t.platform == Platform::Github {
                    &pr["head"]["sha"]
                } else {
                    &pr["sha"]
                })?
                .into(),
            );
            s.pr_url = Some(
                pr[if t.platform == Platform::Github {
                    "html_url"
                } else {
                    "web_url"
                }]
                .as_str()
                .ok_or_else(bad)?
                .into(),
            );
            if s.pr_merged {
                if s.pr_state.as_deref() != Some("closed") {
                    return Err(bad());
                }
                if t.platform == Platform::Github {
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
                } else {
                    let merge = sha(&pr["merge_commit_sha"])?;
                    let root = format!(
                        "projects/{}/repository/commits/{}/refs?type=branch",
                        encode(&t.repository),
                        encode(merge)
                    );
                    let mut names = std::collections::BTreeSet::new();
                    for page in 1..=1000 {
                        let refs = self
                            .transport
                            .request(
                                Method::GET,
                                &format!("{root}&per_page=100&page={page}"),
                                None,
                            )
                            .await?;
                        let refs = refs.as_array().ok_or_else(bad)?;
                        for reference in refs {
                            if reference["type"] != "branch" {
                                return Err(bad());
                            }
                            let name = reference["name"].as_str().ok_or_else(bad)?;
                            if !names.insert(name.to_string()) {
                                return Err(Error::new(
                                    "conflict",
                                    "GitLab commit references changed during pagination",
                                ));
                            }
                        }
                        if refs.len() < 100 {
                            break;
                        }
                        if page == 1000 {
                            return Err(Error::new(
                                "response",
                                "GitLab commit reference pagination limit exceeded",
                            ));
                        }
                    }
                    s.delivered = names.contains(&self.contract.pr_target);
                }
            }
        }
        if t.platform == Platform::Github
            && let Some(project) = self.project()?
        {
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
        if proposal.actions.is_empty()
            || proposal
                .actions
                .iter()
                .any(|action| !proposal.allows(action))
        {
            return Ok(json!({"applied":false,"snapshot":before,"plan":proposal}));
        }
        let mut completed = Vec::new();
        for action in &proposal.actions {
            // Refresh evidence before every mutation, never replay a stale multi-step plan.
            let (latest, p) = self
                .inspect(accepted)
                .await
                .map_err(|e| partial(e, &completed))?;
            if latest.head_sha.as_deref() != Some(expected)
                || p.blockers
                    .iter()
                    .any(|reason| *reason != "human_acceptance_required")
            {
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
            if !p.allows(action) {
                return Err(partial(
                    Error::new(
                        "conflict",
                        "Action requires acceptance; inspect before continuing",
                    ),
                    &completed,
                ));
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
                    .close(CloseReason::Completed)
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
        if !remaining.actions.is_empty()
            || remaining
                .blockers
                .iter()
                .any(|reason| *reason != "human_acceptance_required")
        {
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
