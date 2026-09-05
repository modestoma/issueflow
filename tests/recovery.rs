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
