use async_trait::async_trait;
use http::Method;
use issueflow::{
    config::{Config, Overrides, Platform},
    error::Result,
    hierarchy::{Hierarchy, target_from_url},
    target::Target,
    transport::Transport,
};
use serde_json::{Value, json};
use std::{collections::HashMap, collections::VecDeque, sync::Mutex};

struct Mock {
    replies: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<(Method, String, Option<Value>)>>,
}

#[async_trait]
impl Transport for Mock {
    async fn request(&self, method: Method, endpoint: &str, body: Option<Value>) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((method, endpoint.into(), body));
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request"))
    }
}

fn issue(number: u64, id: u64) -> Value {
    json!({"id":id,"node_id":format!("I{id}"),"number":number,"title":"Issue","body":"Body","state":"open","labels":[],"created_at":"now","updated_at":"now","html_url":format!("https://github.com/owner/repo/issues/{number}")})
}

fn target(number: u64) -> Target {
    Target {
        platform: Platform::Github,
        repository: "owner/repo".into(),
        number: Some(number),
    }
}

#[tokio::test]
async fn reads_parent_and_paginated_children() {
    let mock = Mock {
        replies: Mutex::new(vec![issue(1, 10), json!([issue(3, 30)])].into()),
        calls: Mutex::new(vec![]),
    };
    let hierarchy = Hierarchy {
        transport: &mock,
        parent: target(2),
    };
    assert_eq!(hierarchy.parent().await.unwrap()["number"], 1);
    assert_eq!(hierarchy.children().await.unwrap()[0]["number"], 3);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls[0].1, "repos/owner/repo/issues/2/parent");
    assert_eq!(
        calls[1].1,
        "repos/owner/repo/issues/2/sub_issues?per_page=100&page=1"
    );
}

#[tokio::test]
async fn add_child_checks_cycles_writes_once_and_reads_back() {
    let child = issue(3, 30);
    let mock = Mock {
        replies: Mutex::new(
            vec![
                json!([]),
                child.clone(),
                json!([]),
                json!({}),
                json!([child]),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .add_child(&target(3))
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    let calls = mock.calls.lock().unwrap();
    let writes: Vec<_> = calls
        .iter()
        .filter(|(method, _, _)| *method != Method::GET)
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].1, "repos/owner/repo/issues/2/sub_issues");
    assert_eq!(writes[0].2.as_ref().unwrap()["sub_issue_id"], 30);
}

#[tokio::test]
async fn github_remove_child_writes_once_and_reads_back() {
    let child = issue(3, 30);
    let mock = Mock {
        replies: Mutex::new(vec![child.clone(), json!([child]), json!({}), json!([])].into()),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .remove_child(&target(3))
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls[2].0, Method::DELETE);
    assert_eq!(calls[2].1, "repos/owner/repo/issues/2/sub_issue");
    assert_eq!(calls[2].2.as_ref().unwrap()["sub_issue_id"], 30);
}

#[tokio::test]
async fn github_existing_and_missing_relationships_are_noops() {
    let child = issue(3, 30);
    let add_mock = Mock {
        replies: Mutex::new(vec![json!([]), child.clone(), json!([child.clone()])].into()),
        calls: Mutex::new(vec![]),
    };
    assert_eq!(
        Hierarchy {
            transport: &add_mock,
            parent: target(2)
        }
        .add_child(&target(3))
        .await
        .unwrap()["changed"],
        false
    );
    assert!(
        add_mock
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _, _)| *method == Method::GET)
    );

    let remove_mock = Mock {
        replies: Mutex::new(vec![child, json!([])].into()),
        calls: Mutex::new(vec![]),
    };
    assert_eq!(
        Hierarchy {
            transport: &remove_mock,
            parent: target(2)
        }
        .remove_child(&target(3))
        .await
        .unwrap()["changed"],
        false
    );
    assert!(
        remove_mock
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _, _)| *method == Method::GET)
    );
}

#[tokio::test]
async fn cycle_is_rejected_before_write() {
    let mock = Mock {
        replies: Mutex::new(vec![json!([issue(2, 20)])].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .add_child(&target(3))
    .await
    .unwrap_err();
    assert_eq!(error.code, "conflict");
    assert!(
        mock.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _, _)| *method == Method::GET)
    );
}

#[test]
fn gitlab_hierarchy_urls_are_rejected_before_transport_creation() {
    let config = Config::resolve(
        HashMap::new(),
        HashMap::from([
            (
                "ISSUEFLOW_GITLAB_URL".into(),
                "https://gitlab.example".into(),
            ),
            ("ISSUEFLOW_GITLAB_TOKEN".into(), "secret".into()),
        ]),
        Overrides::default(),
    )
    .unwrap();

    for url in [
        "https://gitlab.example/group/repo/-/issues/2",
        "https://gitlab.example/group/repo/-/work_items/3",
    ] {
        let error = target_from_url(&config, url).unwrap_err();
        assert_eq!(error.code, "input");
        assert!(error.message.contains("ordinary issues"));
        assert!(!error.outcome_unknown);
    }
}
