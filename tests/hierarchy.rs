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
use std::collections::HashMap;
use std::{collections::VecDeque, sync::Mutex};

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
fn gitlab_target(number: u64) -> Target {
    Target {
        platform: Platform::Gitlab,
        repository: "group/repo".into(),
        number: Some(number),
    }
}
fn gitlab_item(id: u64, iid: u64, kind: &str, parent: Option<Value>) -> Value {
    json!({"data":{"namespace":{"workItem":{"id":format!("gid://gitlab/WorkItem/{id}"),"iid":iid.to_string(),"title":kind,"webUrl":format!("https://gitlab.example/group/repo/-/work_items/{iid}"),"workItemType":{"name":kind},"widgets":[{"type":"HIERARCHY","parent":parent,"children":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}]}}}})
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
                Value::Null,
                json!([]),
                child.clone(),
                json!([]),
                json!({}),
                json!([child]),
                issue(2, 20),
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
        replies: Mutex::new(vec![issue(2, 20), json!([child.clone()])].into()),
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
        replies: Mutex::new(vec![Value::Null, json!([issue(2, 20)])].into()),
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

#[tokio::test]
async fn recursive_sub_issues_preserve_depth_position_and_cross_repository_urls() {
    let mut cross_repo = issue(4, 40);
    cross_repo["html_url"] = json!("https://github.com/owner/other/issues/4");
    let mock = Mock {
        replies: Mutex::new(
            vec![
                json!([issue(3, 30), cross_repo]),
                json!([issue(5, 50)]),
                json!([]),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .sub_issues(true, Some(2))
    .await
    .unwrap();
    assert_eq!(result[0]["depth"], 1);
    assert_eq!(result[0]["position"], 1);
    assert_eq!(result[1]["depth"], 2);
    assert_eq!(result[1]["parent_url"], issue(3, 30)["html_url"]);
    assert_eq!(result[2]["url"], "https://github.com/owner/other/issues/4");
    assert_eq!(
        mock.calls.lock().unwrap()[2].1,
        "repos/owner/other/issues/4/sub_issues?per_page=100&page=1"
    );
}

#[tokio::test]
async fn recursive_sub_issues_reject_invalid_depth_and_repeated_nodes() {
    let no_calls = Mock {
        replies: Mutex::new(vec![].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Hierarchy {
        transport: &no_calls,
        parent: target(2),
    }
    .sub_issues(true, Some(0))
    .await
    .unwrap_err();
    assert_eq!(error.code, "input");

    let repeated = Mock {
        replies: Mutex::new(vec![json!([issue(3, 30)]), json!([issue(3, 30)])].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Hierarchy {
        transport: &repeated,
        parent: target(2),
    }
    .sub_issues(true, Some(8))
    .await
    .unwrap_err();
    assert_eq!(error.code, "conflict");
}

#[tokio::test]
async fn more_than_100_direct_sub_issues_is_never_partial_success() {
    let first_page = (1..=100)
        .map(|number| issue(number, number + 1000))
        .collect::<Vec<_>>();
    let mock = Mock {
        replies: Mutex::new(vec![json!(first_page), json!([issue(101, 1101)])].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .children()
    .await
    .unwrap_err();
    assert_eq!(error.code, "conflict");
}

#[tokio::test]
async fn explicit_reparenting_verifies_new_and_old_parents() {
    let child = issue(3, 30);
    let mock = Mock {
        replies: Mutex::new(
            vec![
                issue(1, 10),
                json!([]),
                child.clone(),
                json!([]),
                json!({}),
                json!([child]),
                issue(2, 20),
                json!([]),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .add_child_with_move(&target(3), Some(&target(1)))
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(result["parent"]["number"], 2);
}

#[tokio::test]
async fn adding_child_with_wrong_expected_parent_stops_before_write() {
    let mock = Mock {
        replies: Mutex::new(vec![issue(1, 10)].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .add_child_with_move(&target(3), Some(&target(9)))
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

#[tokio::test]
async fn remove_parent_without_parent_is_a_noop() {
    let mock = Mock {
        replies: Mutex::new(vec![Value::Null].into()),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: target(3),
    }
    .remove_parent()
    .await
    .unwrap();
    assert_eq!(result, json!({"changed":false,"parent":Value::Null}));
}

#[tokio::test]
async fn move_child_writes_exact_native_order_and_reads_back() {
    let child = issue(3, 30);
    let sibling = issue(4, 40);
    let mock = Mock {
        replies: Mutex::new(
            vec![
                json!([child.clone(), sibling.clone()]),
                json!({}),
                json!([sibling, child]),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .move_child(&target(3), None, Some(&target(4)))
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls[1].0, Method::PATCH);
    assert_eq!(calls[1].1, "repos/owner/repo/issues/2/sub_issues/priority");
    assert_eq!(calls[1].2, Some(json!({"sub_issue_id":30,"after_id":40})));
}

#[tokio::test]
async fn relationships_keep_hierarchy_and_dependency_directions_separate() {
    let mut closed = issue(4, 40);
    closed["state"] = json!("closed");
    let blocker = issue(5, 50);
    let blocked = issue(6, 60);
    let mock = Mock {
        replies: Mutex::new(
            vec![
                issue(1, 10),
                json!([issue(3, 30), closed]),
                issue(2, 20),
                json!([blocker]),
                issue(2, 20),
                json!([blocked]),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: target(2),
    }
    .relationships()
    .await
    .unwrap();
    assert_eq!(result["parent"]["number"], 1);
    assert_eq!(result["sub_issues_summary"]["completed"], 1);
    assert_eq!(result["sub_issues_summary"]["total"], 2);
    assert_eq!(result["blocked_by"][0]["number"], 5);
    assert_eq!(result["blocking"][0]["number"], 6);
}

#[tokio::test]
async fn gitlab_ordering_is_rejected_before_transport() {
    let mock = Mock {
        replies: Mutex::new(vec![].into()),
        calls: Mutex::new(vec![]),
    };
    let error = Hierarchy {
        transport: &mock,
        parent: gitlab_target(2),
    }
    .move_child(&gitlab_target(3), Some(&gitlab_target(4)), None)
    .await
    .unwrap_err();
    assert_eq!(error.code, "input");
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn gitlab_reads_parent_and_children_with_work_item_types() {
    let response = json!({"data":{"namespace":{"workItem":{"id":"gid://gitlab/WorkItem/2","iid":"2","title":"Issue","webUrl":"https://gitlab.example/group/repo/-/issues/2","workItemType":{"name":"Issue"},"widgets":[{"type":"HIERARCHY","parent":{"id":"gid://gitlab/WorkItem/1","iid":"1","title":"Epic","webUrl":"https://gitlab.example/groups/group/-/epics/1","workItemType":{"name":"Epic"}},"children":{"nodes":[{"id":"gid://gitlab/WorkItem/3","iid":"3","title":"Task","webUrl":"https://gitlab.example/group/repo/-/work_items/3","workItemType":{"name":"Task"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}]}}}});
    let mock = Mock {
        replies: Mutex::new(vec![response.clone(), response].into()),
        calls: Mutex::new(vec![]),
    };
    let hierarchy = Hierarchy {
        transport: &mock,
        parent: Target {
            platform: Platform::Gitlab,
            repository: "group/repo".into(),
            number: Some(2),
        },
    };
    assert_eq!(
        hierarchy.parent().await.unwrap()["workItemType"]["name"],
        "Epic"
    );
    assert_eq!(
        hierarchy.children().await.unwrap()[0]["workItemType"]["name"],
        "Task"
    );
    let calls = mock.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .all(|(method, endpoint, _)| *method == Method::POST && endpoint == "graphql")
    );
    assert_eq!(
        calls[0].2.as_ref().unwrap()["variables"]["fullPath"],
        "group/repo"
    );
}

#[tokio::test]
async fn gitlab_adds_issue_task_parent_and_reads_back() {
    let parent_value = json!({"id":"gid://gitlab/WorkItem/2","iid":"2","title":"Issue","webUrl":"https://gitlab.example/group/repo/-/issues/2","workItemType":{"name":"Issue"}});
    let mock = Mock {
        replies: Mutex::new(vec![
            gitlab_item(2, 2, "Issue", None),
            gitlab_item(3, 3, "Task", None),
            json!({"data":{"workItemUpdate":{"workItem":{"id":"gid://gitlab/WorkItem/3"},"errors":[]}}}),
            gitlab_item(3, 3, "Task", Some(parent_value)),
        ].into()),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: gitlab_target(2),
    }
    .add_child(&gitlab_target(3))
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls[2].2.as_ref().unwrap()["variables"]["input"]["id"],
        "gid://gitlab/WorkItem/3"
    );
    assert_eq!(
        calls[2].2.as_ref().unwrap()["variables"]["input"]["hierarchyWidget"]["parentId"],
        "gid://gitlab/WorkItem/2"
    );
}

#[tokio::test]
async fn gitlab_removes_issue_task_parent_and_reads_back() {
    let parent_value = json!({"id":"gid://gitlab/WorkItem/2","iid":"2","title":"Issue","webUrl":"https://gitlab.example/group/repo/-/issues/2","workItemType":{"name":"Issue"}});
    let mock = Mock {
        replies: Mutex::new(vec![
            gitlab_item(2, 2, "Issue", None),
            gitlab_item(3, 3, "Task", Some(parent_value)),
            json!({"data":{"workItemUpdate":{"workItem":{"id":"gid://gitlab/WorkItem/3"},"errors":[]}}}),
            gitlab_item(3, 3, "Task", None),
        ].into()),
        calls: Mutex::new(vec![]),
    };
    let result = Hierarchy {
        transport: &mock,
        parent: gitlab_target(2),
    }
    .remove_child(&gitlab_target(3))
    .await
    .unwrap();
    assert_eq!(result["changed"], true);
    assert_eq!(
        mock.calls.lock().unwrap()[2].2.as_ref().unwrap()["variables"]["input"]["hierarchyWidget"]
            ["parentId"],
        Value::Null
    );
}

#[tokio::test]
async fn gitlab_rejects_issue_to_issue_without_mutation() {
    let mock = Mock {
        replies: Mutex::new(
            vec![
                gitlab_item(2, 2, "Issue", None),
                gitlab_item(3, 3, "Issue", None),
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
    };
    let error = Hierarchy {
        transport: &mock,
        parent: gitlab_target(2),
    }
    .add_child(&gitlab_target(3))
    .await
    .unwrap_err();
    assert_eq!(error.code, "input");
    assert_eq!(mock.calls.lock().unwrap().len(), 2);
}

#[test]
fn gitlab_work_item_url_requires_configured_host_and_base_path() {
    let config = Config::resolve(
        HashMap::new(),
        HashMap::from([(
            "ISSUEFLOW_GITLAB_URL".into(),
            "https://gitlab.example/tools".into(),
        )]),
        Overrides::default(),
    )
    .unwrap();
    let target = target_from_url(
        &config,
        "https://gitlab.example/tools/group/repo/-/work_items/3",
    )
    .unwrap();
    assert_eq!(target.platform, Platform::Gitlab);
    assert_eq!(target.repository, "group/repo");
    assert_eq!(target.number, Some(3));
    for invalid in [
        "https://evil.example/tools/group/repo/-/work_items/3",
        "https://gitlab.example/group/repo/-/work_items/3",
        "https://user@gitlab.example/tools/group/repo/-/work_items/3",
    ] {
        assert!(target_from_url(&config, invalid).is_err());
    }
}
