use async_trait::async_trait;
use http::Method;
use issueflow::{
    config::{Config, Overrides, Platform},
    error::{Error, Result},
    service::{CloseReason, CreateInput, Service, Stage, UpdateInput},
    target::Target,
    transport::Transport,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

struct Step {
    method: Method,
    path: String,
    body: Option<Value>,
    response: Value,
}
struct Mock(Mutex<VecDeque<Step>>);
impl Mock {
    fn new(steps: Vec<Step>) -> Self {
        Self(Mutex::new(steps.into()))
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert!(self.0.lock().unwrap().is_empty(), "unconsumed requests");
        }
    }
}
#[async_trait]
impl Transport for Mock {
    async fn request(&self, method: Method, endpoint: &str, body: Option<Value>) -> Result<Value> {
        let step = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        assert_eq!(method, step.method);
        assert_eq!(endpoint, step.path);
        assert_eq!(body, step.body);
        Ok(step.response)
    }
}
fn step(method: Method, path: &str, body: Option<Value>, response: Value) -> Step {
    Step {
        method,
        path: path.into(),
        body,
        response,
    }
}
fn issue(platform: Platform, labels: &[&str]) -> Value {
    if platform == Platform::Github {
        json!({"id": 10, "number": 2, "title":"test", "body":"old", "state":"open", "labels":labels, "html_url":"https://github.com/owner/repo/issues/2", "updated_at":"old-time"})
    } else {
        json!({"id":99,"iid":2,"title":"test","description":"old","state":"opened","labels":labels,"web_url":"https://gitlab.example/group/sub/repo/-/issues/2","updated_at":"old-time"})
    }
}
fn target(platform: Platform) -> Target {
    Target {
        platform,
        repository: if platform == Platform::Github {
            "owner/repo"
        } else {
            "group/sub/repo"
        }
        .into(),
        number: Some(2),
    }
}

#[test]
fn links_override_default_repo_but_reject_foreign_hosts_and_prs() {
    let config = Config::resolve(
        HashMap::from([(
            "ISSUEFLOW_GITLAB_URL".into(),
            "https://gitlab.example/tools/".into(),
        )]),
        HashMap::new(),
        Overrides::default(),
    )
    .unwrap();
    let t = Target::from_url(
        &config,
        "https://gitlab.example/tools/group/sub/repo/-/issues/42#note_8",
    )
    .unwrap();
    assert_eq!(t.repository, "group/sub/repo");
    assert_eq!(
        t.endpoint().unwrap(),
        "projects/group%2Fsub%2Frepo/issues/42"
    );
    for url in [
        "https://evil.test/owner/repo/issues/2",
        "https://github.com/owner/repo/pull/2",
        "https://user:secret@github.com/owner/repo/issues/2",
    ] {
        assert!(Target::from_url(&config, url).is_err());
    }
}

#[tokio::test]
async fn list_fetches_closed_and_all_pages_excluding_prs() {
    let mut page: Vec<_> = (1..=100)
        .map(|id| {
            let mut v = issue(Platform::Github, &[]);
            v["id"] = json!(id);
            v
        })
        .collect();
    page[0]["pull_request"] = json!({});
    let mock = Mock::new(vec![
        step(
            Method::GET,
            "repos/owner/repo/issues?state=all&sort=created&direction=asc&per_page=100&page=1",
            None,
            json!(page),
        ),
        step(
            Method::GET,
            "repos/owner/repo/issues?state=all&sort=created&direction=asc&per_page=100&page=2",
            None,
            json!([]),
        ),
    ]);
    let service = Service {
        transport: &mock,
        target: target(Platform::Github),
    };
    assert_eq!(service.list().await.unwrap().as_array().unwrap().len(), 99);
}

#[tokio::test]
async fn gitlab_create_maps_body_labels_and_iid() {
    let op = "550e8400-e29b-41d4-a716-446655440000";
    let mock = Mock::new(vec![
        step(
            Method::GET,
            "projects/group%2Fsub%2Frepo/issues?state=all&scope=all&order_by=created_at&sort=asc&per_page=100&page=1",
            None,
            json!([]),
        ),
        step(
            Method::POST,
            "projects/group%2Fsub%2Frepo/issues",
            Some(
                json!({"title":"test","description":format!("body\n\n<!-- issueflow-operation: {op} -->"),"labels":"type::bug"}),
            ),
            issue(Platform::Gitlab, &["type::bug"]),
        ),
    ]);
    let service = Service {
        transport: &mock,
        target: target(Platform::Gitlab),
    };
    let result = service
        .create(
            CreateInput {
                title: "test".into(),
                body: "body".into(),
                labels: vec!["type::bug".into()],
            },
            op,
        )
        .await
        .unwrap();
    assert_eq!(result["issue"]["number"], 2);
    assert_eq!(result["issue"]["id"], 99);
}

#[tokio::test]
async fn existing_operation_does_not_post_again() {
    let op = "550e8400-e29b-41d4-a716-446655440000";
    let mut existing = issue(Platform::Github, &[]);
    existing["body"] = json!(format!("body\n\n<!-- issueflow-operation: {op} -->"));
    let mock = Mock::new(vec![step(
        Method::GET,
        "repos/owner/repo/issues?state=all&sort=created&direction=asc&per_page=100&page=1",
        None,
        json!([existing]),
    )]);
    let result = Service {
        transport: &mock,
        target: target(Platform::Github),
    }
    .create(
        CreateInput {
            title: "test".into(),
            body: "body".into(),
            labels: vec![],
        },
        op,
    )
    .await
    .unwrap();
    assert_eq!(result["reused"], true);
}

#[tokio::test]
async fn update_rejects_stale_version_without_write() {
    let mock = Mock::new(vec![step(
        Method::GET,
        "repos/owner/repo/issues/2",
        None,
        issue(Platform::Github, &[]),
    )]);
    let error = Service {
        transport: &mock,
        target: target(Platform::Github),
    }
    .update(
        UpdateInput {
            body: Some("new".into()),
            ..Default::default()
        },
        Some("stale"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "conflict");
}

#[tokio::test]
async fn gitlab_updates_only_explicit_fields() {
    let mock = Mock::new(vec![
        step(
            Method::GET,
            "projects/group%2Fsub%2Frepo/issues/2",
            None,
            issue(Platform::Gitlab, &[]),
        ),
        step(
            Method::PUT,
            "projects/group%2Fsub%2Frepo/issues/2",
            Some(json!({"description":""})),
            issue(Platform::Gitlab, &[]),
        ),
    ]);
    Service {
        transport: &mock,
        target: target(Platform::Gitlab),
    }
    .update(
        UpdateInput {
            body: Some("".into()),
            ..Default::default()
        },
        Some("old-time"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn labels_preserve_unrelated_labels_and_use_targeted_operations() {
    let initial = issue(Platform::Github, &["team::a", "old"]);
    let final_issue = issue(Platform::Github, &["team::a", "new"]);
    let mock = Mock::new(vec![
        step(Method::GET, "repos/owner/repo/issues/2", None, initial),
        step(
            Method::POST,
            "repos/owner/repo/issues/2/labels",
            Some(json!({"labels":["new"]})),
            json!([]),
        ),
        step(
            Method::DELETE,
            "repos/owner/repo/issues/2/labels/old",
            None,
            Value::Null,
        ),
        step(Method::GET, "repos/owner/repo/issues/2", None, final_issue),
    ]);
    let result = Service {
        transport: &mock,
        target: target(Platform::Github),
    }
    .labels(vec!["new".into()], vec!["old".into()])
    .await
    .unwrap();
    assert_eq!(result["labels"], json!(["team::a", "new"]));
}

#[tokio::test]
async fn closed_issue_is_not_silently_reopened_by_transition() {
    let mut value = issue(Platform::Github, &[]);
    value["state"] = json!("closed");
    let mock = Mock::new(vec![step(
        Method::GET,
        "repos/owner/repo/issues/2",
        None,
        value,
    )]);
    assert!(
        Service {
            transport: &mock,
            target: target(Platform::Github)
        }
        .transition(Stage::Ready)
        .await
        .is_err()
    );
}

#[tokio::test]
async fn dependency_cycle_blocks_write() {
    let mock = Mock::new(vec![
        step(
            Method::GET,
            "repos/owner/repo/issues/3",
            None,
            issue(Platform::Github, &[]),
        ),
        step(
            Method::GET,
            "repos/owner/repo/issues/3/dependencies/blocked_by?per_page=100&page=1",
            None,
            json!([issue(Platform::Github, &[])]),
        ),
    ]);
    let mut blocker = target(Platform::Github);
    blocker.number = Some(3);
    let error = Service {
        transport: &mock,
        target: target(Platform::Github),
    }
    .add_dependency(&blocker)
    .await
    .unwrap_err();
    assert_eq!(error.code, "conflict");
}

#[test]
fn errors_do_not_claim_failed_write_was_not_applied() {
    assert!(Error::network(true).outcome_unknown);
    assert!(Error::http(503, true).outcome_unknown);
    assert!(!Error::http(401, true).outcome_unknown);
}

#[tokio::test]
async fn gitlab_transition_preserves_team_label_and_single_stage() {
    let path = "projects/group%2Fsub%2Frepo/issues/2";
    let initial = issue(Platform::Gitlab, &["team::a", "workflow::就绪"]);
    let updated = issue(Platform::Gitlab, &["team::a", "workflow::开发中"]);
    let mock = Mock::new(vec![
        step(Method::GET, path, None, initial.clone()),
        step(Method::GET, path, None, initial.clone()),
        step(Method::GET, path, None, initial),
        step(
            Method::PUT,
            path,
            Some(json!({"add_labels":"workflow::开发中","remove_labels":"workflow::就绪"})),
            updated.clone(),
        ),
        step(Method::GET, path, None, updated),
    ]);
    let result = Service {
        transport: &mock,
        target: target(Platform::Gitlab),
    }
    .transition(Stage::InProgress)
    .await
    .unwrap();
    assert_eq!(result["state"], "open");
    assert_eq!(result["labels"], json!(["team::a", "workflow::开发中"]));
}

#[tokio::test]
async fn closing_completed_maps_labels_and_platform_state() {
    let path = "projects/group%2Fsub%2Frepo/issues/2";
    let initial = issue(Platform::Gitlab, &["workflow::待验收", "team::a"]);
    let labeled = issue(Platform::Gitlab, &["workflow::已完成", "team::a"]);
    let mut closed = labeled.clone();
    closed["state"] = json!("closed");
    let mock = Mock::new(vec![
        step(Method::GET, path, None, initial.clone()),
        step(Method::GET, path, None, initial.clone()),
        step(Method::GET, path, None, initial),
        step(
            Method::PUT,
            path,
            Some(json!({"add_labels":"workflow::已完成","remove_labels":"workflow::待验收"})),
            labeled.clone(),
        ),
        step(Method::GET, path, None, labeled),
        step(
            Method::PUT,
            path,
            Some(json!({"state_event":"close"})),
            closed,
        ),
    ]);
    assert_eq!(
        Service {
            transport: &mock,
            target: target(Platform::Gitlab)
        }
        .close(CloseReason::Completed)
        .await
        .unwrap()["state"],
        "closed"
    );
}

#[tokio::test]
async fn reopening_clears_resolution_and_returns_to_triage() {
    let path = "projects/group%2Fsub%2Frepo/issues/2";
    let opened = issue(
        Platform::Gitlab,
        &["workflow::已终止", "resolution::取消", "team::a"],
    );
    let mut closed = opened.clone();
    closed["state"] = json!("closed");
    let triage = issue(Platform::Gitlab, &["workflow::待复查", "team::a"]);
    let mock = Mock::new(vec![
        step(Method::GET, path, None, closed),
        step(
            Method::PUT,
            path,
            Some(json!({"state_event":"reopen"})),
            opened.clone(),
        ),
        step(Method::GET, path, None, opened.clone()),
        step(Method::GET, path, None, opened.clone()),
        step(Method::GET, path, None, opened),
        step(
            Method::PUT,
            path,
            Some(
                json!({"add_labels":"workflow::待复查","remove_labels":"workflow::已终止,resolution::取消"}),
            ),
            triage.clone(),
        ),
        step(Method::GET, path, None, triage),
    ]);
    let result = Service {
        transport: &mock,
        target: target(Platform::Gitlab),
    }
    .reopen()
    .await
    .unwrap();
    assert_eq!(result["labels"], json!(["workflow::待复查", "team::a"]));
}

#[tokio::test]
async fn conflicting_workflow_labels_are_rejected_before_write() {
    let mock = Mock::new(vec![step(
        Method::GET,
        "repos/owner/repo/issues/2",
        None,
        issue(Platform::Github, &["workflow::就绪"]),
    )]);
    assert!(
        Service {
            transport: &mock,
            target: target(Platform::Github)
        }
        .labels(vec!["workflow::开发中".into()], vec![])
        .await
        .is_err()
    );
}

#[tokio::test]
async fn gitlab_removes_link_id_not_issue_id() {
    let path = "projects/group%2Fsub%2Frepo/issues/2";
    let list = "projects/group%2Fsub%2Frepo/issues/2/links?per_page=100&page=1";
    let mut blocker = issue(Platform::Gitlab, &[]);
    blocker["iid"] = json!(3);
    blocker["id"] = json!(999);
    blocker["issue_link_id"] = json!(17);
    blocker["link_type"] = json!("is_blocked_by");
    blocker["references"] = json!({"full":"group/sub/repo#3"});
    let mock = Mock::new(vec![
        step(Method::GET, path, None, issue(Platform::Gitlab, &[])),
        step(Method::GET, list, None, json!([blocker])),
        step(
            Method::DELETE,
            "projects/group%2Fsub%2Frepo/issues/2/links/17",
            None,
            Value::Null,
        ),
        step(Method::GET, path, None, issue(Platform::Gitlab, &[])),
        step(Method::GET, list, None, json!([])),
    ]);
    let mut target_blocker = target(Platform::Gitlab);
    target_blocker.number = Some(3);
    assert_eq!(
        Service {
            transport: &mock,
            target: target(Platform::Gitlab)
        }
        .remove_dependency(&target_blocker)
        .await
        .unwrap()["removed"],
        true
    );
}

#[tokio::test]
async fn unknown_input_fields_and_empty_edits_are_rejected() {
    assert!(serde_json::from_value::<CreateInput>(json!({"title":"test","typo":"bad"})).is_err());
    let mock = Mock::new(vec![]);
    assert!(
        Service {
            transport: &mock,
            target: target(Platform::Github)
        }
        .update(UpdateInput::default(), None)
        .await
        .is_err()
    );
}
