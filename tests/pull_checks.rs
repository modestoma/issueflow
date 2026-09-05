use issueflow::pull_checks::summarize;
use serde_json::json;
#[test]
fn missing_checks_are_not_passed() {
    let v = summarize("head", &[], &[], &[]).unwrap();
    assert_eq!(v["observed_checks"], "absent");
    assert_eq!(v["merge_authorized"], false);
}
#[test]
fn latest_status_supersedes_prior_failure() {
    let v = summarize(
        "head",
        &[],
        &[
            json!({"id":1,"context":"ci","state":"failure"}),
            json!({"id":2,"context":"ci","state":"success"}),
        ],
        &[],
    )
    .unwrap();
    assert_eq!(v["observed_checks"], "passed");
}
#[test]
fn old_reviews_are_tagged() {
    let v = summarize(
        "new",
        &[],
        &[],
        &[
            json!({"commit_id":"old","state":"APPROVED"}),
            json!({"commit_id":"new","state":"CHANGES_REQUESTED"}),
        ],
    )
    .unwrap();
    assert_eq!(v["reviews"][0]["matches_head"], false);
    assert_eq!(v["reviews"][1]["matches_head"], true);
}
#[test]
fn separates_pending_failed_and_skipped() {
    for (check, result) in [
        (json!({"status":"in_progress"}), "pending"),
        (
            json!({"status":"completed","conclusion":"failure"}),
            "failed",
        ),
        (
            json!({"status":"completed","conclusion":"skipped"}),
            "non_failing",
        ),
    ] {
        assert_eq!(
            summarize("head", &[check], &[], &[]).unwrap()["observed_checks"],
            result
        );
    }
}
use async_trait::async_trait;
use http::Method;
use issueflow::{
    config::Platform, error::Result, pull_checks::inspect, target::Target, transport::Transport,
};
use serde_json::Value;
use std::{collections::VecDeque, sync::Mutex};
struct Mock(Mutex<VecDeque<Value>>);
#[async_trait]
impl Transport for Mock {
    async fn request(&self, m: Method, _: &str, _: Option<Value>) -> Result<Value> {
        assert_eq!(m, Method::GET);
        Ok(self.0.lock().unwrap().pop_front().unwrap())
    }
}
#[tokio::test]
async fn head_changes_reject_stale_evidence() {
    let pr = |sha: &str| json!({"number":6,"head":{"sha":sha},"base":{"ref":"main"}});
    let m = Mock(Mutex::new(
        vec![
            pr(&"a".repeat(40)),
            json!({"check_runs":[]}),
            json!([]),
            json!([]),
            pr(&"b".repeat(40)),
        ]
        .into(),
    ));
    let e = inspect(
        &m,
        Target {
            platform: Platform::Github,
            repository: "owner/repo".into(),
            number: Some(6),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "conflict");
}

#[tokio::test]
async fn unrelated_repository_metadata_does_not_invalidate_evidence() {
    let before = json!({"number":6,"head":{"sha":"a".repeat(40)},"base":{"ref":"main","sha":"b".repeat(40),"repo":{"id":1,"description":"old"}}});
    let mut after = before.clone();
    after["base"]["repo"]["description"] = json!("new");
    let m = Mock(Mutex::new(
        vec![
            before,
            json!({"check_runs":[]}),
            json!([]),
            json!([]),
            after,
        ]
        .into(),
    ));
    let v = inspect(
        &m,
        Target {
            platform: Platform::Github,
            repository: "owner/repo".into(),
            number: Some(6),
        },
    )
    .await
    .unwrap();
    assert_eq!(v["observed_checks"], "absent");
}

#[tokio::test]
async fn gitlab_reports_pipeline_and_approval_evidence_without_authorizing_merge() {
    let mr = json!({
        "iid":9,
        "state":"opened",
        "draft":false,
        "sha":"a".repeat(40),
        "diff_refs":{"head_sha":"a".repeat(40)},
        "source_branch":"feat/change",
        "target_branch":"main",
        "source_project_id":4,
        "target_project_id":4
    });
    let approvals = json!({"approvals_required":1,"approvals_left":0,"approved_by":[{"user":{"username":"reviewer"}}]});
    let m = Mock(Mutex::new(
        vec![
            mr.clone(),
            json!([{"id":2,"status":"success","sha":"a".repeat(40)}]),
            approvals.clone(),
            mr,
        ]
        .into(),
    ));
    let v = inspect(
        &m,
        Target {
            platform: Platform::Gitlab,
            repository: "group/sub/repo".into(),
            number: Some(9),
        },
    )
    .await
    .unwrap();
    assert_eq!(v["observed_checks"], "passed");
    assert_eq!(v["approvals"], approvals);
    assert_eq!(v["merge_authorized"], false);
}
