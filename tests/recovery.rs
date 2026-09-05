use async_trait::async_trait;
use http::Method;
use issueflow::{
    branch_contract::BranchContract,
    config::{Config, Overrides},
    error::Result,
    recovery::{Recovery, Snapshot, plan},
    transport::Transport,
    workflow_config::{DeliveryPolicy, WorkflowConfig},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};
fn completed() -> Snapshot {
    Snapshot {
        issue_state: "open".into(),
        pr_state: Some("closed".into()),
        pr_merged: true,
        delivered: true,
        head_sha: Some("a".repeat(40)),
        project_configured: true,
        in_project: true,
        done_option_available: true,
        project_status: Some("Done".into()),
        metadata_ready: true,
        resolution: Some("Completed".into()),
        ..Default::default()
    }
}
#[test]
fn acceptance_required_never_closes_from_merge_alone() {
    let p = plan(&completed(), DeliveryPolicy::AcceptanceRequired, false);
    assert_eq!(p.phase, "acceptance_pending");
    assert!(p.actions.is_empty());
}
#[test]
fn merged_policy_closes_only_missing_issue_step() {
    assert_eq!(
        plan(&completed(), DeliveryPolicy::Merged, false).actions,
        vec!["close_issue"]
    );
}
#[test]
fn closed_issue_with_stale_project_only_repairs_project() {
    let mut s = completed();
    s.issue_state = "closed".into();
    s.issue_reason = Some("completed".into());
    s.project_configured = true;
    s.in_project = true;
    s.done_option_available = true;
    s.project_status = Some("In review".into());
    assert_eq!(
        plan(&s, DeliveryPolicy::Merged, false).actions,
        vec!["set_project_done"]
    );
}
#[test]
fn missing_membership_is_ordered_before_status_and_close() {
    let mut s = completed();
    s.in_project = false;
    s.project_status = None;
    s.project_configured = true;
    s.done_option_available = true;
    assert_eq!(
        plan(&s, DeliveryPolicy::Merged, false).actions,
        vec!["add_to_project", "set_project_done", "close_issue"]
    );
}
#[test]
fn terminated_issues_are_never_reopened_or_completed() {
    let mut s = completed();
    s.issue_state = "closed".into();
    s.issue_reason = Some("not_planned".into());
    let p = plan(&s, DeliveryPolicy::Merged, true);
    assert_eq!(p.phase, "manual_review");
    assert!(p.actions.is_empty());
}
#[test]
fn missing_target_delivery_and_done_option_block_writes() {
    let mut s = completed();
    s.delivered = false;
    assert!(plan(&s, DeliveryPolicy::Merged, true).actions.is_empty());
    s.delivered = true;
    s.project_configured = true;
    s.done_option_available = false;
    assert!(plan(&s, DeliveryPolicy::Merged, true).actions.is_empty());
}
#[test]
fn already_complete_is_noop() {
    let mut s = completed();
    s.issue_state = "closed".into();
    s.issue_reason = Some("completed".into());
    assert_eq!(plan(&s, DeliveryPolicy::Merged, false).phase, "complete");
}
struct Mock {
    steps: Mutex<VecDeque<(Method, Value)>>,
    calls: Mutex<Vec<(Method, String)>>,
}
#[async_trait]
impl Transport for Mock {
    async fn request(&self, m: Method, p: &str, body: Option<Value>) -> Result<Value> {
        if m == Method::POST
            && p == "graphql"
            && body
                .as_ref()
                .and_then(|v| v["query"].as_str())
                .is_some_and(|q| q.starts_with("query"))
        {
            let body = body.unwrap();
            let q = body["query"].as_str().unwrap();
            if q.contains("owner:user") {
                return Ok(
                    json!({"data":{"owner":{"projectV2":{"id":"P1","closed":false,"title":"Board"}}}}),
                );
            }
            if q.contains("fields(first:") {
                return Ok(
                    json!({"data":{"node":{"fields":{"nodes":[{"id":"F1","name":"Status","options":[{"id":"O1","name":"Done"}]},{"id":"T","name":"Work type","options":[]},{"id":"P","name":"Priority","options":[]},{"id":"B","name":"Blocked","options":[]},{"id":"R","name":"Resolution","options":[]}],"pageInfo":{"hasNextPage":false}}}}}),
                );
            }
            if q.contains("items(first:") {
                return Ok(
                    json!({"data":{"node":{"items":{"nodes":[{"id":"ITEM1","content":{"id":"I1"},"isArchived":false,"fieldValueByName":{"name":"Done"},"resolution":{"name":"Completed"},"blocked":{"name":"No"}}],"pageInfo":{"hasNextPage":false}}}}}),
                );
            }
            panic!("unexpected GraphQL query");
        }
        let (expected, v) = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        assert_eq!(m, expected);
        assert!(!p.ends_with("/merge") && !p.ends_with("/comments"));
        self.calls.lock().unwrap().push((m.clone(), p.into()));
        if v["simulate_failure"] == true {
            return Err(issueflow::error::Error::network(m != Method::GET));
        }
        Ok(v)
    }
}
fn cfg() -> Config {
    Config::resolve(HashMap::new(), HashMap::new(), Overrides::default()).unwrap()
}
fn gitlab_cfg() -> Config {
    Config::resolve(
        HashMap::new(),
        HashMap::from([(
            "ISSUEFLOW_GITLAB_URL".into(),
            "https://gitlab.example".into(),
        )]),
        Overrides::default(),
    )
    .unwrap()
}
fn gitlab_workflow(policy: &str) -> WorkflowConfig {
    serde_json::from_value(json!({"schema_version":1,"platform":"gitlab","host":"gitlab.example","repository":"group/sub/repo","remote":"origin","base_branch":"main","timezone":"Asia/Shanghai","delivery_policy":policy,"permissions":{"local_commit":true,"push":true,"pull_request":true}})).unwrap()
}
fn gitlab_contract() -> BranchContract {
    serde_json::from_value(json!({"schema_version":1,"issue_url":"https://gitlab.example/group/sub/repo/-/issues/1","source_branch":"main","branch":"feat/change","pr_target":"main","pr_url":"https://gitlab.example/group/sub/repo/-/merge_requests/2"})).unwrap()
}
fn gitlab_issue() -> Value {
    json!({"id":1,"iid":1,"web_url":"https://gitlab.example/group/sub/repo/-/issues/1","state":"opened","labels":["type::feature","priority::P1","workflow::待验收"]})
}
fn gitlab_completed_issue() -> Value {
    json!({"id":1,"iid":1,"web_url":"https://gitlab.example/group/sub/repo/-/issues/1","title":"Issue","description":"Body","created_at":"now","updated_at":"now","state":"closed","labels":["type::feature","priority::P1","workflow::已完成"]})
}
fn gitlab_mr() -> Value {
    json!({"id":2,"iid":2,"web_url":"https://gitlab.example/group/sub/repo/-/merge_requests/2","state":"merged","merged_at":"now","draft":false,"sha":"a".repeat(40),"source_branch":"feat/change","target_branch":"main","source_project_id":7,"target_project_id":7,"merge_commit_sha":"b".repeat(40),"description":"Refs https://gitlab.example/group/sub/repo/-/issues/1"})
}
fn gitlab_target_refs() -> Value {
    json!([{"type":"branch","name":"main"}])
}
fn workflow() -> WorkflowConfig {
    serde_json::from_value(json!({"schema_version":1,"platform":"github","host":"github.com","repository":"a/b","remote":"origin","base_branch":"main","timezone":"Asia/Shanghai","delivery_policy":"merged","github_project_url":"https://github.com/users/a/projects/1","permissions":{"local_commit":true,"push":true}})).unwrap()
}
fn contract() -> BranchContract {
    serde_json::from_value(json!({"schema_version":1,"issue_url":"https://github.com/a/b/issues/1","source_branch":"main","branch":"feat/issue-1","pr_target":"main","pr_url":"https://github.com/a/b/pull/2"})).unwrap()
}
fn issue(closed: bool) -> Value {
    json!({"id":1,"node_id":"I1","number":1,"html_url":"https://github.com/a/b/issues/1","state":if closed{"closed"}else{"open"},"state_reason":if closed{Some("completed")}else{None},"labels":[{"name":"type::feature"},{"name":"workflow::已完成"},{"name":"resolution::取消"},{"name":"blocked"}]})
}
fn pr() -> Value {
    json!({"number":2,"html_url":"https://github.com/a/b/pull/2","state":"closed","merged":true,"draft":false,"head":{"sha":"a".repeat(40),"ref":"feat/issue-1","repo":{"full_name":"a/b"}},"base":{"ref":"main"},"merge_commit_sha":"b".repeat(40),"body":"Refs https://github.com/a/b/issues/1"})
}
fn compare() -> Value {
    json!({"status":"ahead","merge_base_commit":{"sha":"b".repeat(40)}})
}
fn snapshot(closed: bool) -> Vec<(Method, Value)> {
    vec![
        (Method::GET, issue(closed)),
        (Method::GET, pr()),
        (Method::GET, compare()),
    ]
}
#[tokio::test]
async fn default_reconcile_only_reads() {
    let m = Mock {
        steps: Mutex::new(snapshot(false).into()),
        calls: Mutex::new(vec![]),
    };
    let cfg = cfg();
    let c = contract();
    let w = workflow();
    let v = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, false, None)
    .await
    .unwrap();
    assert_eq!(v["applied"], false);
    assert_eq!(v["plan"]["actions"], json!(["close_issue"]));
    assert!(m.steps.lock().unwrap().is_empty());
}
#[tokio::test]
async fn gitlab_merged_delivery_plans_close_without_project_requests() {
    let m = Mock {
        steps: Mutex::new(
            vec![
                (Method::GET, gitlab_issue()),
                (Method::GET, gitlab_mr()),
                (Method::GET, gitlab_target_refs()),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let cfg = gitlab_cfg();
    let c = gitlab_contract();
    let w = gitlab_workflow("merged");
    let v = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, false, None)
    .await
    .unwrap();
    assert_eq!(v["snapshot"]["label_workflow"], true);
    assert_eq!(v["snapshot"]["delivered"], true);
    assert_eq!(v["plan"]["actions"], json!(["close_issue"]));
    assert!(
        m.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, path)| path != "graphql")
    );
}

#[tokio::test]
async fn gitlab_acceptance_policy_never_closes_from_merge_alone() {
    let m = Mock {
        steps: Mutex::new(
            vec![
                (Method::GET, gitlab_issue()),
                (Method::GET, gitlab_mr()),
                (Method::GET, gitlab_target_refs()),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let cfg = gitlab_cfg();
    let c = gitlab_contract();
    let w = gitlab_workflow("acceptance_required");
    let v = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, false, None)
    .await
    .unwrap();
    assert_eq!(v["plan"]["phase"], "acceptance_pending");
    assert_eq!(v["plan"]["actions"], json!([]));
}

#[tokio::test]
async fn gitlab_apply_closes_once_after_expected_head_and_reads_back() {
    let completed = gitlab_completed_issue();
    let completed_open = json!({"id":1,"iid":1,"web_url":"https://gitlab.example/group/sub/repo/-/issues/1","title":"Issue","description":"Body","created_at":"now","updated_at":"now","state":"opened","labels":["type::feature","priority::P1","workflow::已完成"]});
    let mut steps = vec![
        (Method::GET, gitlab_issue()),
        (Method::GET, gitlab_mr()),
        (Method::GET, gitlab_target_refs()),
    ];
    steps.extend([
        (Method::GET, gitlab_issue()),
        (Method::GET, gitlab_mr()),
        (Method::GET, gitlab_target_refs()),
    ]);
    steps.extend([
        (Method::GET, gitlab_issue()),
        (Method::GET, gitlab_issue()),
        (Method::GET, gitlab_issue()),
        (Method::PUT, json!({})),
        (Method::GET, completed_open),
        (Method::PUT, completed.clone()),
    ]);
    steps.extend([
        (Method::GET, completed),
        (Method::GET, gitlab_mr()),
        (Method::GET, gitlab_target_refs()),
    ]);
    let m = Mock {
        steps: Mutex::new(steps.into()),
        calls: Mutex::new(vec![]),
    };
    let cfg = gitlab_cfg();
    let c = gitlab_contract();
    let w = gitlab_workflow("merged");
    let v = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, true, Some(&"a".repeat(40)))
    .await
    .unwrap();
    assert_eq!(v["plan"]["phase"], "complete");
    let writes: Vec<_> = m
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| *method == Method::PUT)
        .cloned()
        .collect();
    assert_eq!(writes.len(), 2);
    assert!(
        writes
            .iter()
            .all(|(_, path)| path == "projects/group%2Fsub%2Frepo/issues/1")
    );
}
#[tokio::test]
async fn apply_only_closes_issue_without_merging_or_commenting() {
    let mut steps = snapshot(false);
    steps.extend(snapshot(false));
    steps.extend([
        (Method::GET, issue(false)),
        (Method::PATCH, json!({})),
        (Method::GET, issue(true)),
    ]);
    steps.extend(snapshot(true));
    let m = Mock {
        steps: Mutex::new(steps.into()),
        calls: Mutex::new(vec![]),
    };
    let cfg = cfg();
    let c = contract();
    let w = workflow();
    let v = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, true, Some(&"a".repeat(40)))
    .await
    .unwrap();
    assert_eq!(v["plan"]["phase"], "complete");
    assert_eq!(
        m.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 != Method::GET)
            .count(),
        1
    );
    assert!(m.steps.lock().unwrap().is_empty());
}
#[tokio::test]
async fn changed_expected_head_prevents_writes() {
    let m = Mock {
        steps: Mutex::new(snapshot(false).into()),
        calls: Mutex::new(vec![]),
    };
    let cfg = cfg();
    let c = contract();
    let w = workflow();
    let e = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, true, Some(&"c".repeat(40)))
    .await
    .unwrap_err();
    assert_eq!(e.code, "conflict");
}
#[tokio::test]
async fn historical_discovery_uses_all_pr_states() {
    let mut c = contract();
    c.pr_url = None;
    let mut candidate = pr();
    candidate["id"] = json!(2);
    let mut steps = vec![
        (Method::GET, issue(true)),
        (Method::GET, json!([candidate])),
    ];
    steps.extend([(Method::GET, pr()), (Method::GET, compare())]);
    let m = Mock {
        steps: Mutex::new(steps.into()),
        calls: Mutex::new(vec![]),
    };
    let cfg = cfg();
    let w = workflow();
    let v = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, false, None)
    .await
    .unwrap();
    assert_eq!(v["plan"]["phase"], "complete");
    assert!(m.calls.lock().unwrap()[1].1.contains("state=all"));
}

#[tokio::test]
async fn failed_close_readback_recovers_without_second_write() {
    let mut steps = snapshot(false);
    steps.extend(snapshot(false));
    steps.extend([
        (Method::GET, issue(false)),
        (Method::PATCH, json!({})),
        (Method::GET, json!({"simulate_failure":true})),
    ]);
    let m = Mock {
        steps: Mutex::new(steps.into()),
        calls: Mutex::new(vec![]),
    };
    let cfg = cfg();
    let c = contract();
    let w = workflow();
    let e = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, true, Some(&"a".repeat(40)))
    .await
    .unwrap_err();
    assert!(e.outcome_unknown);
    let mut steps = snapshot(true);
    steps.extend(snapshot(true));
    let m = Mock {
        steps: Mutex::new(steps.into()),
        calls: Mutex::new(vec![]),
    };
    let v = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    }
    .reconcile(false, true, Some(&"a".repeat(40)))
    .await
    .unwrap();
    assert_eq!(v["applied"], false);
    assert_eq!(v["plan"]["phase"], "complete");
    assert!(m.calls.lock().unwrap().iter().all(|v| v.0 == Method::GET));
}
#[test]
fn malformed_native_state_never_proposes_mutation() {
    let mut s = completed();
    s.issue_state = "unknown".into();
    assert!(plan(&s, DeliveryPolicy::Merged, true).actions.is_empty());
}

#[test]
fn missing_project_blocks_recovery_instead_of_falling_back_to_labels() {
    let mut s = completed();
    s.project_configured = false;
    let p = plan(&s, DeliveryPolicy::Merged, true);
    assert_eq!(p.phase, "setup_required");
    assert!(p.actions.is_empty());
}
#[test]
fn project_blocked_and_resolution_are_authoritative() {
    let mut s = completed();
    s.project_blocked = true;
    assert!(plan(&s, DeliveryPolicy::Merged, true).actions.is_empty());
    s.project_blocked = false;
    s.has_resolution = true;
    assert_eq!(
        plan(&s, DeliveryPolicy::Merged, true).phase,
        "manual_review"
    );
}

#[test]
fn pending_acceptance_plans_project_repairs_only() {
    let mut s = completed();
    s.in_project = false;
    s.project_status = None;
    s.resolution = None;
    let p = plan(&s, DeliveryPolicy::AcceptanceRequired, false);
    assert_eq!(p.phase, "acceptance_pending");
    assert_eq!(p.actions, vec!["add_to_project", "set_project_done"]);
    assert_eq!(p.blockers, vec!["human_acceptance_required"]);
    s.done_option_available = false;
    assert!(
        plan(&s, DeliveryPolicy::AcceptanceRequired, false)
            .actions
            .is_empty()
    );
}

struct AcceptanceMock {
    member: Mutex<bool>,
    done: Mutex<bool>,
    writes: Mutex<Vec<String>>,
    pr_reads: Mutex<usize>,
    change_head: bool,
    fail_done: bool,
}
impl AcceptanceMock {
    fn new() -> Self {
        Self {
            member: Mutex::new(false),
            done: Mutex::new(false),
            writes: Mutex::new(vec![]),
            pr_reads: Mutex::new(0),
            change_head: false,
            fail_done: false,
        }
    }
}
#[async_trait]
impl Transport for AcceptanceMock {
    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        if path == "graphql" {
            assert_eq!(method, Method::POST);
            let body = body.unwrap();
            let q = body["query"].as_str().unwrap();
            if q.starts_with("mutation") {
                self.writes.lock().unwrap().push(q.into());
                if q.contains("addProjectV2ItemById") {
                    *self.member.lock().unwrap() = true;
                    return Ok(json!({"data":{"addProjectV2ItemById":{"item":{"id":"ITEM1"}}}}));
                }
                assert!(q.contains("updateProjectV2ItemFieldValue"));
                assert_eq!(body["variables"]["field"], "F1", "must not set Resolution");
                assert_eq!(body["variables"]["option"], "O1");
                *self.done.lock().unwrap() = true;
                if self.fail_done {
                    return Err(issueflow::error::Error::network(true));
                }
                return Ok(
                    json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"ITEM1"}}}}),
                );
            }
            assert!(q.starts_with("query"));
            if q.contains("items(first:") {
                let nodes = if *self.member.lock().unwrap() {
                    let done = *self.done.lock().unwrap();
                    json!([{"id":"ITEM1","content":{"id":"I1"},"isArchived":false,
                        "fieldValueByName":{"name":if done {"Done"}else{"In review"},"optionId":if done {"O1"}else{"O2"}},
                        "resolution":null,"blocked":{"name":"No"}}])
                } else {
                    json!([])
                };
                return Ok(
                    json!({"data":{"node":{"items":{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}}}),
                );
            }
            if q.contains("repository(owner:") {
                return Ok(json!({"data":{"repository":{"issue":{"id":"I1"}}}}));
            }
            // Reuse the ordinary metadata fixture for read-only Project queries.
            let mock = Mock {
                steps: Mutex::new(VecDeque::new()),
                calls: Mutex::new(vec![]),
            };
            return mock.request(method, path, Some(body)).await;
        }
        assert_eq!(method, Method::GET, "must not close or mutate the issue");
        Ok(if path == "repos/a/b/issues/1" {
            issue(false)
        } else if path == "repos/a/b/pulls/2" {
            let mut reads = self.pr_reads.lock().unwrap();
            *reads += 1;
            let mut p = pr();
            if self.change_head && *reads >= 3 {
                p["head"]["sha"] = json!("c".repeat(40));
            }
            p
        } else if path.starts_with("repos/a/b/compare/") {
            compare()
        } else {
            panic!("unexpected request: {path}");
        })
    }
}
#[tokio::test]
async fn pending_acceptance_applies_done_without_closure_and_repeats_safely() {
    let m = AcceptanceMock::new();
    let cfg = cfg();
    let c = contract();
    let mut w = workflow();
    w.delivery_policy = DeliveryPolicy::AcceptanceRequired;
    let recovery = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    };
    let preview = recovery.reconcile(false, false, None).await.unwrap();
    assert_eq!(
        preview["plan"]["actions"],
        json!(["add_to_project", "set_project_done"])
    );
    assert!(m.writes.lock().unwrap().is_empty());
    let result = recovery
        .reconcile(false, true, Some(&"a".repeat(40)))
        .await
        .unwrap();
    assert_eq!(
        result["completed_actions"],
        json!(["add_to_project", "set_project_done"])
    );
    assert_eq!(result["plan"]["phase"], "acceptance_pending");
    assert_eq!(result["snapshot"]["issue_state"], "open");
    assert_eq!(result["snapshot"]["resolution"], Value::Null);
    let again = recovery
        .reconcile(false, true, Some(&"a".repeat(40)))
        .await
        .unwrap();
    assert_eq!(again["applied"], false);
    assert_eq!(m.writes.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn head_change_between_project_repairs_stops_remaining_writes() {
    let mut m = AcceptanceMock::new();
    m.change_head = true;
    let cfg = cfg();
    let c = contract();
    let mut w = workflow();
    w.delivery_policy = DeliveryPolicy::AcceptanceRequired;
    let recovery = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    };
    let err = recovery
        .reconcile(false, true, Some(&"a".repeat(40)))
        .await
        .unwrap_err();
    assert_eq!(err.code, "conflict");
    assert_eq!(m.writes.lock().unwrap().len(), 1);
    assert!(!*m.done.lock().unwrap());
}
#[tokio::test]
async fn unknown_done_write_recovers_without_repeating_mutation() {
    let mut m = AcceptanceMock::new();
    m.fail_done = true;
    let cfg = cfg();
    let c = contract();
    let mut w = workflow();
    w.delivery_policy = DeliveryPolicy::AcceptanceRequired;
    let recovery = Recovery {
        config: &cfg,
        transport: &m,
        contract: &c,
        parent: None,
        workflow: &w,
    };
    assert!(
        recovery
            .reconcile(false, true, Some(&"a".repeat(40)))
            .await
            .unwrap_err()
            .outcome_unknown
    );
    let result = recovery
        .reconcile(false, true, Some(&"a".repeat(40)))
        .await
        .unwrap();
    assert_eq!(result["applied"], false);
    assert_eq!(result["snapshot"]["project_status"], "Done");
    assert_eq!(m.writes.lock().unwrap().len(), 2);
}
