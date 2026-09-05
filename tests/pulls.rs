use async_trait::async_trait;
use http::Method;
use issueflow::{
    config::{Config, Overrides, Platform},
    error::Result,
    pull::{CreatePull, MergeMethod, Pulls, UpdatePull, target_from_url},
    service::{CloseReason, Service},
    target::Target,
    transport::Transport,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};
struct Mock {
    replies: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<(Method, String, Option<Value>)>>,
}
impl Mock {
    fn new(v: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(v.into()),
            calls: Mutex::new(vec![]),
        }
    }
}
#[async_trait]
impl Transport for Mock {
    async fn request(&self, m: Method, p: &str, b: Option<Value>) -> Result<Value> {
        self.calls.lock().unwrap().push((m, p.into(), b));
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request"))
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
            "https://gitlab.example/tools".into(),
        )]),
        Overrides::default(),
    )
    .unwrap()
}
fn gitlab_target() -> Target {
    Target {
        platform: Platform::Gitlab,
        repository: "group/sub/repo".into(),
        number: Some(9),
    }
}
fn mr() -> Value {
    json!({"id":90,"iid":9,"state":"opened","draft":false,"title":"Add feature","description":"Refs https://gitlab.example/tools/group/sub/repo/-/issues/5","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","diff_refs":{"head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","base_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"source_branch":"feat/issue-5","target_branch":"feat/parent","source_project_id":4,"target_project_id":4})
}
fn target() -> Target {
    Target {
        platform: Platform::Github,
        repository: "owner/repo".into(),
        number: Some(5),
    }
}
fn pr() -> Value {
    json!({"id":1,"number":8,"state":"open","draft":false,"merged":false,"body":"Refs https://github.com/owner/repo/issues/5","head":{"ref":"feat/issue-5","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","repo":{"full_name":"owner/repo"}},"base":{"ref":"feat/parent"}})
}
fn input() -> CreatePull {
    serde_json::from_value(json!({"title":"Add feature","body":"Implemented and tested.","head":"feat/issue-5","base":"feat/parent"})).unwrap()
}
#[test]
fn validates_pr_url() {
    assert_eq!(
        target_from_url(&cfg(), "https://github.com/owner/repo/pull/8")
            .unwrap()
            .number,
        Some(8)
    );
    for s in [
        "https://evil.test/owner/repo/pull/8",
        "https://github.com/owner/repo/issues/8",
        "https://github.com/owner/repo/pull/0",
    ] {
        assert!(target_from_url(&cfg(), s).is_err());
    }
}
#[test]
fn validates_configured_gitlab_mr_url() {
    let target = target_from_url(
        &gitlab_cfg(),
        "https://gitlab.example/tools/group/sub/repo/-/merge_requests/9",
    )
    .unwrap();
    assert_eq!(target.platform, Platform::Gitlab);
    assert_eq!(target.repository, "group/sub/repo");
    assert_eq!(target.number, Some(9));
    assert!(
        target_from_url(
            &gitlab_cfg(),
            "https://evil.example/tools/group/sub/repo/-/merge_requests/9"
        )
        .is_err()
    );
}
#[tokio::test]
async fn gitlab_create_maps_fields_and_reads_back() {
    let m = Mock::new(vec![
        json!({"id":5,"iid":5,"title":"Issue","description":"Body","state":"opened","labels":[],"created_at":"now","updated_at":"now","web_url":"https://gitlab.example/tools/group/sub/repo/-/issues/5"}),
        json!([]),
        json!({"iid":9}),
        mr(),
    ]);
    let v = Pulls {
        transport: &m,
        target: gitlab_target(),
    }
    .create(
        input(),
        "https://gitlab.example/tools/group/sub/repo/-/issues/5",
    )
    .await
    .unwrap();
    assert_eq!(v["reused"], false);
    let calls = m.calls.lock().unwrap();
    assert_eq!(
        calls[1].1,
        "projects/group%2Fsub%2Frepo/merge_requests?state=opened&order_by=created_at&sort=asc&source_branch=feat%2Fissue-5&target_branch=feat%2Fparent&per_page=100&page=1"
    );
    let payload = calls[2].2.as_ref().unwrap();
    assert_eq!(payload["source_branch"], "feat/issue-5");
    assert_eq!(payload["target_branch"], "feat/parent");
    assert!(
        payload["description"]
            .as_str()
            .unwrap()
            .contains("Refs https://")
    );
}
#[tokio::test]
async fn gitlab_update_uses_put_and_preserves_reference() {
    let mut edited = mr();
    edited["description"] =
        json!("Changed\n\nRefs https://gitlab.example/tools/group/sub/repo/-/issues/5");
    let m = Mock::new(vec![mr(), json!({}), edited]);
    Pulls {
        transport: &m,
        target: gitlab_target(),
    }
    .update(
        UpdatePull {
            title: None,
            body: Some("Changed".into()),
        },
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .await
    .unwrap();
    let calls = m.calls.lock().unwrap();
    assert_eq!(calls[1].0, Method::PUT);
    assert!(
        calls[1].2.as_ref().unwrap()["description"]
            .as_str()
            .unwrap()
            .contains("Refs https://")
    );
}
#[tokio::test]
async fn gitlab_merge_sends_expected_sha_and_verifies() {
    let mut merged = mr();
    merged["state"] = json!("merged");
    merged["merged_at"] = json!("now");
    let m = Mock::new(vec![mr(), merged.clone(), merged]);
    let v = Pulls {
        transport: &m,
        target: gitlab_target(),
    }
    .merge(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "feat/parent",
        MergeMethod::Squash,
    )
    .await
    .unwrap();
    assert_eq!(v["merged"], true);
    let calls = m.calls.lock().unwrap();
    assert_eq!(
        calls[1].1,
        "projects/group%2Fsub%2Frepo/merge_requests/9/merge"
    );
    assert_eq!(
        calls[1].2.as_ref().unwrap()["sha"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(calls[1].2.as_ref().unwrap()["squash"], true);
}
#[tokio::test]
async fn child_pr_targets_parent_and_links_issue_without_closing() {
    let m = Mock::new(vec![
        json!({"id":5,"number":5,"html_url":"https://github.com/owner/repo/issues/5","labels":[]}),
        json!([]),
        json!({"number":8}),
        pr(),
    ]);
    let v = Pulls {
        transport: &m,
        target: target(),
    }
    .create(input(), "https://github.com/owner/repo/issues/5")
    .await
    .unwrap();
    assert_eq!(v["reused"], false);
    let c = m.calls.lock().unwrap();
    assert_eq!(c[2].0, Method::POST);
    assert_eq!(c[2].1, "repos/owner/repo/pulls");
    let b = c[2].2.as_ref().unwrap();
    assert_eq!(b["base"], "feat/parent");
    assert!(
        b["body"]
            .as_str()
            .unwrap()
            .ends_with("Refs https://github.com/owner/repo/issues/5")
    );
    assert_eq!(c[3].1, "repos/owner/repo/pulls/8");
}
#[tokio::test]
async fn reuses_open_pr_without_post() {
    let m = Mock::new(vec![
        json!({"id":5,"number":5,"html_url":"https://github.com/owner/repo/issues/5","labels":[]}),
        json!([pr()]),
    ]);
    assert_eq!(
        Pulls {
            transport: &m,
            target: target()
        }
        .create(input(), "https://github.com/owner/repo/issues/5")
        .await
        .unwrap()["reused"],
        true
    );
    assert!(m.calls.lock().unwrap().iter().all(|c| c.0 == Method::GET));
}
#[tokio::test]
async fn rejects_unrelated_existing_pr() {
    let mut v = pr();
    v["body"] = json!("Different issue");
    let m = Mock::new(vec![
        json!({"id":5,"number":5,"html_url":"https://github.com/owner/repo/issues/5","labels":[]}),
        json!([v]),
    ]);
    assert_eq!(
        Pulls {
            transport: &m,
            target: target()
        }
        .create(input(), "https://github.com/owner/repo/issues/5")
        .await
        .unwrap_err()
        .code,
        "conflict"
    );
}
#[tokio::test]
async fn rejects_stale_head_wrong_base_and_draft_without_write() {
    for (sha, base, draft) in [
        (
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "feat/parent",
            false,
        ),
        ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "main", false),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "feat/parent",
            true,
        ),
    ] {
        let mut v = pr();
        v["draft"] = json!(draft);
        let m = Mock::new(vec![v]);
        assert!(
            Pulls {
                transport: &m,
                target: target()
            }
            .merge(sha, base, MergeMethod::Merge)
            .await
            .is_err()
        );
        assert_eq!(m.calls.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn merge_sends_sha_and_verifies_result() {
    let mut merged = pr();
    merged["state"] = json!("closed");
    merged["merged"] = json!(true);
    let m = Mock::new(vec![pr(), json!({"merged":true}), merged]);
    assert_eq!(
        Pulls {
            transport: &m,
            target: target()
        }
        .merge(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "feat/parent",
            MergeMethod::Merge
        )
        .await
        .unwrap()["merged"],
        true
    );
    let c = m.calls.lock().unwrap();
    assert_eq!(c[1].0, Method::PUT);
    assert_eq!(
        c[1].2.as_ref().unwrap()["sha"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
}
#[tokio::test]
async fn unconfirmed_merge_is_not_success() {
    let m = Mock::new(vec![pr(), json!({"merged":false})]);
    let e = Pulls {
        transport: &m,
        target: target(),
    }
    .merge(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "feat/parent",
        MergeMethod::Merge,
    )
    .await
    .unwrap_err();
    assert!(e.outcome_unknown);
}
#[tokio::test]
async fn invalid_branches_never_reach_api() {
    for head in ["--oops", "x..y", "a.lock", "a/../b", "owner:branch"] {
        let m = Mock::new(vec![]);
        let mut i = input();
        i.head = head.into();
        assert!(
            Pulls {
                transport: &m,
                target: target()
            }
            .create(i, "url")
            .await
            .is_err()
        );
        assert!(m.calls.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn native_close_and_reopen_preserve_labels() {
    for reason in [Some(CloseReason::Completed), None] {
        let state = if reason.is_some() { "closed" } else { "open" };
        let issue = json!({"id":5,"number":5,"html_url":"https://github.com/owner/repo/issues/5","title":"Issue","body":"Body","state":state,"labels":[{"name":"type::feature"}],"created_at":"now","updated_at":"now"});
        let m = Mock::new(vec![issue.clone(), json!({}), issue]);
        let v = Service {
            transport: &m,
            target: target(),
        }
        .native_state(reason)
        .await
        .unwrap();
        assert_eq!(v["labels"], json!(["type::feature"]));
        let c = m.calls.lock().unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[1].1, "repos/owner/repo/issues/5");
        assert!(c[1].2.as_ref().unwrap().get("labels").is_none());
    }
}

#[tokio::test]
async fn update_preserves_reference_and_unspecified_title() {
    let mut edited = pr();
    edited["body"] = json!("New plan\n\nRefs https://github.com/owner/repo/issues/5");
    let m = Mock::new(vec![pr(), json!({}), edited]);
    Pulls {
        transport: &m,
        target: target(),
    }
    .update(
        UpdatePull {
            title: None,
            body: Some("New plan".into()),
        },
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .await
    .unwrap();
    let calls = m.calls.lock().unwrap();
    let payload = calls[1].2.as_ref().unwrap();
    assert!(payload.get("title").is_none());
    assert!(payload["body"].as_str().unwrap().contains("Refs https://"));
}
#[tokio::test]
async fn ready_checks_sha_before_mutation() {
    let m = Mock::new(vec![pr()]);
    assert!(
        Pulls {
            transport: &m,
            target: target()
        }
        .ready("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        .await
        .is_err()
    );
    assert_eq!(m.calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn ready_is_idempotent() {
    let m = Mock::new(vec![pr()]);
    assert_eq!(
        Pulls {
            transport: &m,
            target: target()
        }
        .ready("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .await
        .unwrap()["changed"],
        false
    );
}
#[tokio::test]
async fn draft_ready_mutation_uses_node_id() {
    let mut draft = pr();
    draft["draft"] = json!(true);
    draft["node_id"] = json!("PR1");
    let m = Mock::new(vec![
        draft,
        json!({"data":{"markPullRequestReadyForReview":{"pullRequest":{"id":"PR1"}}}}),
        pr(),
    ]);
    assert_eq!(
        Pulls {
            transport: &m,
            target: target()
        }
        .ready("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .await
        .unwrap()["changed"],
        true
    );
    assert_eq!(
        m.calls.lock().unwrap()[1].2.as_ref().unwrap()["variables"]["id"],
        "PR1"
    );
}
#[tokio::test]
async fn draft_graphql_errors_are_not_success() {
    let mut draft = pr();
    draft["draft"] = json!(true);
    draft["node_id"] = json!("PR1");
    let m = Mock::new(vec![draft, json!({"errors":[{"message":"secret"}]})]);
    let e = Pulls {
        transport: &m,
        target: target(),
    }
    .ready("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .await
    .unwrap_err();
    assert!(e.outcome_unknown);
    assert!(!e.message.contains("secret"));
}

#[tokio::test]
async fn github_default_close_preserves_existing_labels() {
    let issue = json!({"id":5,"number":5,"html_url":"https://github.com/owner/repo/issues/5","state":"closed","labels":[{"name":"custom-label"},{"name":"workflow::开发中"}]});
    let m = Mock::new(vec![issue.clone(), json!({}), issue]);
    let v = Service {
        transport: &m,
        target: target(),
    }
    .close(CloseReason::Completed)
    .await
    .unwrap();
    assert_eq!(v["labels"], json!(["custom-label", "workflow::开发中"]));
    let c = m.calls.lock().unwrap();
    assert_eq!(c.len(), 3);
    assert!(c.iter().all(|v| !v.1.ends_with("/labels")));
}

#[tokio::test]
async fn github_default_reopen_preserves_labels() {
    let i = json!({"id":5,"number":5,"html_url":"https://github.com/owner/repo/issues/5","state":"open","labels":[{"name":"custom-label"}]});
    let m = Mock::new(vec![i.clone(), json!({}), i]);
    let v = Service {
        transport: &m,
        target: target(),
    }
    .reopen()
    .await
    .unwrap();
    assert_eq!(v["labels"], json!(["custom-label"]));
    assert_eq!(m.calls.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn github_workflow_label_setup_is_rejected_without_requests() {
    let m = Mock::new(vec![]);
    assert!(
        Service {
            transport: &m,
            target: target()
        }
        .setup_labels()
        .await
        .is_err()
    );
    assert!(m.calls.lock().unwrap().is_empty());
}
